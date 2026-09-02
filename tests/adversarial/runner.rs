use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::{Read as _, Write as _};
use std::os::fd::{BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tempfile::TempDir;
use zeppelin_embed::epoch::{
    ComputeUnits, EmbeddingEpoch, EmbeddingRuntime, EmbeddingTower, EpochId, EpochIdentity,
    EpochTransitionError, Normalization, StoreEpoch,
};
use zeppelin_embed::fts::dict::{TermDictionary, TermInfo};
use zeppelin_embed::fts::fuzzy;
use zeppelin_embed::fts::index::{DEFAULT_FIELD, Document, SegmentIndex};
use zeppelin_embed::fts::phonetic;
use zeppelin_embed::fts::phrase::{self, PhraseQuery};
use zeppelin_embed::fts::prefix;
use zeppelin_embed::fts::search::TermQuery;
use zeppelin_embed::fts::snippet;
use zeppelin_embed::fts::tokenizer::{Analyzer, Profile, TokenizerConfig};
use zeppelin_embed::fusion::{
    FusionError, FusionLeg, HybridQuery, LexicalCandidate, VectorCandidate, execute_hybrid,
};
use zeppelin_embed::graph::search::GraphSearchProfile;
use zeppelin_embed::ingest::{
    DeleteBatch, DocId, DocumentVersion, GraphSearchStats, IngestBatch, IngestDocument,
    IngestError, IngestRetentionCheckpoint, IngestRetentionFaultEffect,
    IngestRetentionFaultKind as ProductIngestRetentionFaultKind, IngestRetentionFaultReceiptV1,
    IngestRetentionIoKind, IngestRetentionOperation as ProductIngestRetentionOperation, Revision,
    RowSource, SearchRequest, StoreLexicalError,
};
use zeppelin_embed::kernels::vector_fault::KernelFaultController;
use zeppelin_embed::kernels::{KernelBackendId, KernelVariant};
use zeppelin_embed::lifecycle::HybridLegTestFault;
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
use zeppelin_embed::lifecycle::{
    CancelToken, Deadline, GraphSearchOptions, ManualMonotonicClock, OpenOptions, QueryControl,
    QueryError, SearchOptions, SearchTier, StorageCleanupReport, StorageFaultPlan,
    StorageFaultReceipt, Store, StoreError, StoreTestDependencies, SystemMonotonicClock,
};
use zeppelin_embed::manifest::EpochMeta;
use zeppelin_embed::manifest::io::{commit_manifest, load_manifest};
use zeppelin_embed::meta::{
    AliveSet, ColumnDefinition, ColumnId, ColumnStoreBuilder, ColumnType, Predicate,
    PredicateValue, RangeBound, RangePredicate, Schema, TIMESTAMP_COLUMN,
};
use zeppelin_embed::planner::{
    FilterMode, FilteredSearchError, MetadataExecutionReceipt, MetadataFeatureDetail,
    MetadataFeatureReceipt, PlanFallback, PlanNode, SegmentBranch, SegmentPlan, SegmentTier,
};
use zeppelin_embed::quant::{
    Bit4Factors, QuantError, RescoreError, RescoreMetric, RescorePool, est_dot_bit4,
    prepare_bit4_query, quantize_bit4, rescore_top_k,
};
use zeppelin_embed::scan::vector_fault::{
    VectorAllocationSite as ProductVectorAllocationSite, VectorCampaign as ProductVectorCampaign,
    VectorFaultEffect as ProductVectorFaultEffect, VectorFaultKind as ProductVectorFaultKind,
    VectorFaultReceipt, VectorFaultSite as ProductVectorFaultSite,
    VectorOperation as ProductVectorOperation, VectorQuantField as ProductVectorQuantField,
    VectorQuantScheme as ProductVectorQuantScheme, VectorRowSource as ProductVectorRowSource,
    VectorSearchTier as ProductVectorSearchTier,
};
use zeppelin_embed::scan::{ScanError, ScanOptions};
use zeppelin_embed::segment::writer::{
    SegmentBuild, SegmentDocumentVersions, SegmentFactors, write_segment_with_documents,
};
use zeppelin_embed::segment::{MetadataDecodeProvenance, SegmentId};
use zeppelin_embed::tier::{MaintenanceBudget, MaintenanceStatus, TierThresholds};
use zeppelin_embed::vfs::StdVfs;
use zeppelin_embed_adversarial_oracle::diagnostics_health as diagnostics_oracle;
use zeppelin_embed_adversarial_oracle::ffi_bindings as ffi_oracle;
use zeppelin_embed_adversarial_oracle::fts as fts_oracle;
use zeppelin_embed_adversarial_oracle::hybrid_fusion as hybrid_oracle;
use zeppelin_embed_adversarial_oracle::ingest_retention as ingest_oracle;
use zeppelin_embed_adversarial_oracle::lifecycle_accounting as lifecycle_oracle;
use zeppelin_embed_adversarial_oracle::metadata_filter_planner as metadata_oracle;
use zeppelin_embed_adversarial_oracle::storage_durability as storage_oracle;
use zeppelin_embed_adversarial_oracle::tiering_maintenance as tier_oracle;
use zeppelin_embed_adversarial_oracle::vamana_graph as graph_oracle;
use zeppelin_embed_adversarial_oracle::vector_execution as vector_oracle;

use super::artifacts::RunArtifacts;
use super::campaign::{CampaignKind, FaultPlan};
use super::coverage::CoverageRegistry;
use super::diagnostics_health as diagnostics_adapter;
use super::fault_vfs::{self, FaultEvent, FaultSchedule};
use super::ffi_bindings as ffi_adapter;
use super::fts as fts_adapter;
use super::hybrid_fusion as hybrid_adapter;
use super::ingest_retention as ingest_adapter;
use super::lifecycle_accounting as lifecycle_adapter;
use super::metadata_filter_planner as metadata_adapter;
use super::model::{ExpectedHit, Model, ModelEpoch};
use super::oracle::{OracleFirstDifference, OracleRecord};
use super::profiles::{FaultProfile, environment_for_profile, profile_for_seed};
use super::program::{self, Op, Program, SearchKind};
use super::storage_durability as storage_adapter;
use super::tiering_maintenance as tier_adapter;
use super::vamana_graph as graph_adapter;
use super::vector_execution as vector_adapter;

const THREAD_BUDGET: usize = 1;
const NUMERIC_COLUMN: ColumnId = ColumnId::new(1);
const BOOLEAN_COLUMN: ColumnId = ColumnId::new(2);
const STRING_COLUMN: ColumnId = ColumnId::new(3);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FrozenFixtureFile {
    pub(crate) relative_path: PathBuf,
    pub(crate) bytes: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FrozenStoreFixture {
    files: Vec<FrozenFixtureFile>,
    open_options: OpenOptions,
    fault_event: Option<FaultEvent>,
    current_operation: usize,
}

pub(crate) struct IsolatedStoreDirectories {
    pub(crate) clean: TempDir,
    pub(crate) faulted: TempDir,
}

#[derive(Debug)]
pub(crate) struct ControlOutcome<T> {
    pub(crate) clean: Result<T, String>,
    pub(crate) faulted: Result<FaultedLeg<T>, String>,
    pub(crate) same_seed_control_passed: bool,
    pub(crate) fault_event: Option<FaultEvent>,
}

#[derive(Debug)]
pub(crate) enum FaultedLeg<T> {
    Observed(T),
    Refused { stage: &'static str, error: String },
}

pub(crate) struct ControlStore {
    store: Option<Store>,
    directory: PathBuf,
    open_options: OpenOptions,
    dependencies: StoreTestDependencies,
    frozen_files: Vec<FrozenFixtureFile>,
    query_control: Option<QueryControl>,
}

impl ControlStore {
    pub(crate) fn open(
        directory: &Path,
        open_options: OpenOptions,
        dependencies: StoreTestDependencies,
    ) -> Result<Self, String> {
        Self::open_with_frozen_files(directory, open_options, dependencies, None)
    }

    fn open_with_frozen_files(
        directory: &Path,
        open_options: OpenOptions,
        dependencies: StoreTestDependencies,
        frozen_files: Option<Vec<FrozenFixtureFile>>,
    ) -> Result<Self, String> {
        let store = Store::open_with_test_dependencies(
            directory,
            open_options.clone(),
            dependencies.clone(),
        )
        .map_err(|error| error.to_string())?;
        let frozen_files = match frozen_files {
            Some(files) => files,
            None => FrozenStoreFixture::capture(directory)?.files,
        };
        Ok(Self {
            store: Some(store),
            directory: directory.to_path_buf(),
            open_options,
            dependencies,
            frozen_files,
            query_control: None,
        })
    }

    pub(crate) fn store(&self) -> Result<&Store, String> {
        self.store
            .as_ref()
            .ok_or_else(|| "clean-control Store is closed".to_owned())
    }

    pub(crate) fn path(&self) -> &Path {
        &self.directory
    }

    pub(crate) fn scheduled_query_control(&self) -> Option<QueryControl> {
        self.query_control.clone()
    }

    pub(crate) fn close(&mut self) -> Result<(), String> {
        if let Some(store) = self.store.take() {
            store.close().map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    pub(crate) fn reopen(&mut self) -> Result<(), String> {
        self.close()?;
        let store = Store::open_with_test_dependencies(
            &self.directory,
            self.open_options.clone(),
            self.dependencies.clone(),
        )
        .map_err(|error| error.to_string())?;
        self.store = Some(store);
        Ok(())
    }

    pub(crate) fn reset_to_frozen(&mut self) -> Result<(), String> {
        self.close()?;
        for entry in std::fs::read_dir(&self.directory)
            .map_err(|error| format!("read control Store directory for reset: {error}"))?
        {
            let entry = entry.map_err(|error| format!("read control Store entry: {error}"))?;
            let path = entry.path();
            let file_type = entry
                .file_type()
                .map_err(|error| format!("read control Store entry type: {error}"))?;
            if file_type.is_dir() {
                std::fs::remove_dir_all(&path).map_err(|error| {
                    format!("remove control Store directory {}: {error}", path.display())
                })?;
            } else {
                std::fs::remove_file(&path).map_err(|error| {
                    format!("remove control Store file {}: {error}", path.display())
                })?;
            }
        }
        for file in &self.frozen_files {
            let path = self.directory.join(&file.relative_path);
            let parent = path
                .parent()
                .ok_or_else(|| "frozen Store fixture file has no parent".to_owned())?;
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("create reset Store fixture parent: {error}"))?;
            std::fs::write(&path, &file.bytes).map_err(|error| {
                format!("write reset Store fixture file {}: {error}", path.display())
            })?;
        }
        self.reopen()
    }
}

impl FrozenStoreFixture {
    pub(crate) fn capture(root: &Path) -> Result<Self, String> {
        fn collect(
            root: &Path,
            directory: &Path,
            files: &mut Vec<FrozenFixtureFile>,
        ) -> Result<(), String> {
            let mut entries = std::fs::read_dir(directory)
                .map_err(|error| format!("read frozen Store fixture directory: {error}"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| format!("enumerate frozen Store fixture directory: {error}"))?;
            entries.sort_by_key(std::fs::DirEntry::file_name);
            for entry in entries {
                let file_type = entry
                    .file_type()
                    .map_err(|error| format!("read frozen Store fixture file type: {error}"))?;
                let path = entry.path();
                if file_type.is_dir() {
                    collect(root, &path, files)?;
                } else if file_type.is_file() {
                    let relative_path = path
                        .strip_prefix(root)
                        .map_err(|error| format!("relativize frozen Store fixture file: {error}"))?
                        .to_path_buf();
                    let bytes = std::fs::read(&path).map_err(|error| {
                        format!("read frozen Store fixture file {}: {error}", path.display())
                    })?;
                    files.push(FrozenFixtureFile {
                        relative_path,
                        bytes,
                    });
                } else {
                    return Err(format!(
                        "frozen Store fixture contains non-file {}",
                        path.display()
                    ));
                }
            }
            Ok(())
        }

        let mut files = Vec::new();
        collect(root, root, &mut files)?;
        files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        if files.is_empty() {
            return Err("frozen Store fixture contains no files".to_owned());
        }
        Ok(Self {
            files,
            open_options: OpenOptions::default(),
            fault_event: None,
            current_operation: usize::MAX,
        })
    }

    pub(crate) fn with_open_options(mut self, open_options: OpenOptions) -> Self {
        self.open_options = open_options;
        self
    }

    pub(crate) fn with_fault(mut self, event: FaultEvent, current_operation: usize) -> Self {
        self.fault_event = Some(event);
        self.current_operation = current_operation;
        self
    }

    fn for_control(
        open_options: OpenOptions,
        event: Option<FaultEvent>,
        current_operation: usize,
    ) -> Result<Self, String> {
        let source = tempfile::tempdir()
            .map_err(|error| format!("create clean-control source directory: {error}"))?;
        let store = Store::open(source.path(), open_options.clone())
            .map_err(|error| format!("open clean-control source Store: {error}"))?;
        store
            .close()
            .map_err(|error| format!("close clean-control source Store: {error}"))?;
        let fixture = Self::capture(source.path())?.with_open_options(open_options);
        Ok(match event {
            Some(event) => fixture.with_fault(event, current_operation),
            None => fixture,
        })
    }

    pub(crate) fn files(&self) -> &[FrozenFixtureFile] {
        &self.files
    }

    pub(crate) fn materialize(&self) -> Result<TempDir, String> {
        let directory = tempfile::tempdir()
            .map_err(|error| format!("create frozen Store fixture directory: {error}"))?;
        for file in &self.files {
            let path = directory.path().join(&file.relative_path);
            let parent = path
                .parent()
                .ok_or_else(|| "frozen Store fixture file has no parent".to_owned())?;
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("create frozen Store fixture parent: {error}"))?;
            std::fs::write(&path, &file.bytes).map_err(|error| {
                format!(
                    "write frozen Store fixture file {}: {error}",
                    path.display()
                )
            })?;
        }
        let captured = Self::capture(directory.path())?;
        if captured.files != self.files {
            return Err("materialized Store fixture bytes differ from frozen source".to_owned());
        }
        Ok(directory)
    }

    pub(crate) fn isolated_pair(&self) -> Result<IsolatedStoreDirectories, String> {
        let clean = self.materialize()?;
        let faulted = self.materialize()?;
        if clean.path() == faulted.path() {
            return Err("clean and faulted Store fixtures share one directory".to_owned());
        }
        Ok(IsolatedStoreDirectories { clean, faulted })
    }
}

fn finish_control_leg<T>(store: &mut ControlStore, result: Result<T, String>) -> Result<T, String> {
    let closed = store.close();
    match (result, closed) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
    }
}

pub(crate) fn run_with_clean_control<T: std::fmt::Debug>(
    fixture: &FrozenStoreFixture,
    clean: impl FnOnce(&mut ControlStore) -> Result<T, String>,
    faulted: impl FnOnce(&mut ControlStore) -> Result<T, String>,
    clean_oracle: impl FnOnce(&T) -> Result<(), String>,
) -> Result<ControlOutcome<T>, String> {
    let directories = fixture.isolated_pair()?;
    let clean_dependencies =
        StoreTestDependencies::new(Arc::new(StdVfs), Arc::new(SystemMonotonicClock));
    let mut clean_store = ControlStore::open_with_frozen_files(
        directories.clean.path(),
        fixture.open_options.clone(),
        clean_dependencies,
        Some(fixture.files.clone()),
    )
    .map_err(|error| format!("open clean-control Store: {error}"))?;
    let clean_result = clean(&mut clean_store);
    let clean_result = finish_control_leg(&mut clean_store, clean_result)
        .map_err(|error| format!("run clean-control operation: {error}"))?;
    let same_seed_control_passed = clean_oracle(&clean_result).is_ok();

    let scheduled = Arc::new(fault_vfs::std_scheduled(
        fixture
            .fault_event
            .clone()
            .map(FaultSchedule::single)
            .unwrap_or_default(),
    ));
    scheduled.set_operation(fixture.current_operation);
    let fault_control = scheduled.cancel_token()?.map(QueryControl::Cancel);
    let faulted_dependencies =
        StoreTestDependencies::new(scheduled.clone(), Arc::new(SystemMonotonicClock));
    let faulted_result = ControlStore::open_with_frozen_files(
        directories.faulted.path(),
        fixture.open_options.clone(),
        faulted_dependencies,
        Some(fixture.files.clone()),
    );
    let faulted_result = match faulted_result {
        Ok(mut store) => {
            store.query_control = fault_control;
            let result = faulted(&mut store);
            let result = finish_control_leg(&mut store, result);
            match result {
                Ok(observed) => Ok(FaultedLeg::Observed(observed)),
                Err(error) if scheduled.events().into_iter().any(|event| event.fired) => {
                    Ok(FaultedLeg::Refused {
                        stage: "operation",
                        error,
                    })
                }
                Err(error) => Err(error),
            }
        }
        Err(error) if scheduled.events().into_iter().any(|event| event.fired) => {
            Ok(FaultedLeg::Refused {
                stage: "Store::open",
                error,
            })
        }
        Err(error) => Err(error),
    };
    let fault_event = scheduled.events().into_iter().next();
    Ok(ControlOutcome {
        same_seed_control_passed,
        clean: Ok(clean_result),
        faulted: faulted_result,
        fault_event,
    })
}

pub(crate) fn tier_clean_control_for_test(
    operation: tier_adapter::TierOperationKind,
    seed: u64,
) -> Result<bool, String> {
    let fixture =
        FrozenStoreFixture::for_control(tier_adapter::control_open_options(seed), None, 0)?;
    let outcome = run_with_clean_control(
        &fixture,
        |store| tier_adapter::run_tier_operation_on_store(store, operation, seed, None),
        |store| tier_adapter::run_tier_operation_on_store(store, operation, seed, None),
        |evidence| {
            let passed = evidence.invariants.iter().all(|invariant| {
                match invariant {
                    tier_adapter::TierInvariantEvidence::I50 { input, observed } => {
                        tier_oracle::compare_i50(input, observed)
                    }
                    tier_adapter::TierInvariantEvidence::I51 { input, observed } => {
                        tier_oracle::compare_i51(input, observed)
                    }
                    tier_adapter::TierInvariantEvidence::I52 { input, observed } => {
                        tier_oracle::compare_i52(input, observed)
                    }
                    tier_adapter::TierInvariantEvidence::I53 { input, observed } => {
                        tier_oracle::compare_i53(input, observed)
                    }
                }
                .is_ok()
            });
            passed
                .then_some(())
                .ok_or_else(|| "tier oracle disagreed".to_owned())
        },
    )?;
    Ok(outcome.same_seed_control_passed)
}

pub(crate) fn fts_clean_control_for_test(
    operation: fts_adapter::FtsOperationKind,
    seed: u64,
) -> Result<bool, String> {
    let fixture = FrozenStoreFixture::for_control(OpenOptions::default(), None, 0)?;
    let outcome = run_with_clean_control(
        &fixture,
        |store| fts_adapter::run_fts_operation_on_store(store, operation, seed, None),
        |store| fts_adapter::run_fts_operation_on_store(store, operation, seed, None),
        |evidence| {
            let passed = evidence.invariants.iter().all(|invariant| {
                match invariant {
                    fts_adapter::FtsInvariantEvidence::I40 { input, observed } => {
                        fts_oracle::compare_i40(input, observed)
                    }
                    fts_adapter::FtsInvariantEvidence::I41 { input, observed } => {
                        fts_oracle::compare_i41(input, observed)
                    }
                    fts_adapter::FtsInvariantEvidence::I42 { input, observed } => {
                        fts_oracle::compare_i42(input, observed)
                    }
                    fts_adapter::FtsInvariantEvidence::I43 { input, observed } => {
                        fts_oracle::compare_i43(input, observed)
                    }
                    fts_adapter::FtsInvariantEvidence::I44 { input, observed } => {
                        fts_oracle::compare_i44(input, observed)
                    }
                }
                .is_ok()
            });
            passed
                .then_some(())
                .ok_or_else(|| "FTS oracle disagreed".to_owned())
        },
    )?;
    Ok(outcome.same_seed_control_passed)
}

pub(crate) fn graph_clean_control_for_test(
    operation: graph_adapter::GraphOperationKind,
    seed: u64,
) -> Result<bool, String> {
    let fixture =
        FrozenStoreFixture::for_control(graph_adapter::control_open_options(seed), None, 0)?;
    let outcome = run_with_clean_control(
        &fixture,
        |store| graph_adapter::run_graph_operation_on_store(store, operation, seed, None),
        |store| graph_adapter::run_graph_operation_on_store(store, operation, seed, None),
        |evidence| {
            let passed = evidence.invariants.iter().all(|invariant| {
                match invariant {
                    graph_adapter::GraphInvariantEvidence::I28 { input, observed } => {
                        graph_oracle::compare_i28(input, observed)
                    }
                    graph_adapter::GraphInvariantEvidence::I29 { input, observed } => {
                        graph_oracle::compare_i29(input, observed)
                    }
                    graph_adapter::GraphInvariantEvidence::I30 { input, observed } => {
                        graph_oracle::compare_i30(input, observed)
                    }
                    graph_adapter::GraphInvariantEvidence::I31 { input, observed } => {
                        graph_oracle::compare_i31(input, observed)
                    }
                    graph_adapter::GraphInvariantEvidence::I32 { input, observed } => {
                        graph_oracle::compare_i32(input, observed)
                    }
                    graph_adapter::GraphInvariantEvidence::I33 { input, observed } => {
                        graph_oracle::compare_i33(input, observed)
                    }
                    graph_adapter::GraphInvariantEvidence::I34 { input, observed } => {
                        graph_oracle::compare_i34(input, observed)
                    }
                    graph_adapter::GraphInvariantEvidence::I35 { input, observed } => {
                        graph_oracle::compare_i35(input, observed)
                    }
                }
                .is_ok()
            });
            passed
                .then_some(())
                .ok_or_else(|| "graph oracle disagreed".to_owned())
        },
    )?;
    Ok(outcome.same_seed_control_passed)
}

fn adversarial_schema() -> Schema {
    Schema::new(vec![
        ColumnDefinition::new(NUMERIC_COLUMN, "number", ColumnType::U64, true),
        ColumnDefinition::new(BOOLEAN_COLUMN, "flag", ColumnType::Bool, true),
        ColumnDefinition::new(STRING_COLUMN, "class", ColumnType::RawString, true),
    ])
    .expect("adversarial schema is statically valid")
}

fn adversarial_columns(doc_id: u32) -> Vec<(ColumnId, PredicateValue)> {
    let mut columns = vec![
        (
            NUMERIC_COLUMN,
            PredicateValue::U64(program::numeric_column(doc_id)),
        ),
        (
            STRING_COLUMN,
            PredicateValue::String(program::string_column(doc_id).to_owned()),
        ),
    ];
    if let Some(value) = program::boolean_column(doc_id) {
        columns.push((BOOLEAN_COLUMN, PredicateValue::Bool(value)));
    }
    columns
}

fn adversarial_predicate(kind: program::PredicateKind) -> Predicate {
    let numeric_eq = |value| Predicate::Eq {
        column: NUMERIC_COLUMN,
        value: PredicateValue::U64(value),
    };
    let string_eq = |value: &str| Predicate::Eq {
        column: STRING_COLUMN,
        value: PredicateValue::String(value.to_owned()),
    };
    match kind {
        program::PredicateKind::Eq => numeric_eq(1),
        program::PredicateKind::In => Predicate::In {
            column: NUMERIC_COLUMN,
            values: vec![PredicateValue::U64(1), PredicateValue::U64(3)],
        },
        program::PredicateKind::RangeTwoSided => Predicate::Range(RangePredicate {
            column: NUMERIC_COLUMN,
            lower: Some(RangeBound::inclusive(PredicateValue::U64(1))),
            upper: Some(RangeBound::inclusive(PredicateValue::U64(3))),
        }),
        program::PredicateKind::RangeHalfOpen => Predicate::Range(RangePredicate {
            column: NUMERIC_COLUMN,
            lower: Some(RangeBound::inclusive(PredicateValue::U64(2))),
            upper: None,
        }),
        program::PredicateKind::Bool => Predicate::Eq {
            column: BOOLEAN_COLUMN,
            value: PredicateValue::Bool(true),
        },
        program::PredicateKind::String => string_eq("even"),
        program::PredicateKind::Exists => Predicate::Exists(BOOLEAN_COLUMN),
        program::PredicateKind::IsNull => Predicate::IsNull(BOOLEAN_COLUMN),
        program::PredicateKind::And => Predicate::And(vec![
            Predicate::Range(RangePredicate {
                column: NUMERIC_COLUMN,
                lower: Some(RangeBound::inclusive(PredicateValue::U64(1))),
                upper: None,
            }),
            Predicate::Exists(BOOLEAN_COLUMN),
        ]),
        program::PredicateKind::Or => Predicate::Or(vec![numeric_eq(0), string_eq("odd")]),
        program::PredicateKind::Not => Predicate::Not(Box::new(string_eq("odd"))),
    }
}

fn declared_store_epoch() -> StoreEpoch {
    let document = EmbeddingTower {
        model_id: "adversarial-embedding".to_owned(),
        model_version: "1".to_owned(),
        weights_digest: vec![0xad, 0x12],
        dims: program::DIMENSIONS as u32,
        // The harness's own vectors are NOT unit length -- `program::vector`
        // returns arbitrary magnitudes and `Model` scores them with
        // `squared_l2`. Declaring L2 here made the epoch metadata lie about
        // the data, which R06's profile selection correctly refused. Declare
        // what the fixture actually produces.
        normalization: Normalization::None,
        prompt_prefix: "search_document: ".to_owned(),
        max_tokens: 64,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: None,
    };
    let mut query = document.clone();
    query.prompt_prefix = "search_query: ".to_owned();
    StoreEpoch {
        embedding: EmbeddingEpoch {
            document,
            query,
            alignment_digest: Vec::new(),
        },
        tokenizer: TokenizerConfig::text_default().epoch(),
    }
}

fn declared_identity() -> EpochIdentity {
    declared_store_epoch().identity()
}

fn conflicting_identity() -> EpochIdentity {
    let mut epoch = declared_store_epoch();
    epoch.embedding.query.model_version = "2".to_owned();
    epoch.identity()
}

fn epoch_b_store_epoch() -> StoreEpoch {
    let mut epoch = declared_store_epoch();
    epoch.embedding.document.model_version = "2".to_owned();
    epoch.embedding.document.weights_digest = vec![0xbe, 0x21];
    epoch.embedding.query.model_version = "2".to_owned();
    epoch.embedding.query.weights_digest = vec![0xbe, 0x22];
    epoch.embedding.alignment_digest = vec![0xbe, 0x23];
    epoch
}

fn identity_for(epoch: ModelEpoch) -> EpochIdentity {
    match epoch {
        ModelEpoch::A => declared_identity(),
        ModelEpoch::B => epoch_b_store_epoch().identity(),
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Invariant {
    I1,
    I2,
    I3,
    I4,
    I5,
    I6,
    I7,
    I8,
    I9,
    I10,
    I11,
    I12,
    I13,
    I14,
    I54,
    I55,
    Feature(super::campaign::InvariantId),
}

impl Invariant {
    #[must_use]
    pub const fn number(self) -> u8 {
        match self {
            Self::I1 => 1,
            Self::I2 => 2,
            Self::I3 => 3,
            Self::I4 => 4,
            Self::I5 => 5,
            Self::I6 => 6,
            Self::I7 => 7,
            Self::I8 => 8,
            Self::I9 => 9,
            Self::I10 => 10,
            Self::I11 => 11,
            Self::I12 => 12,
            Self::I13 => 13,
            Self::I14 => 14,
            Self::I54 => 54,
            Self::I55 => 55,
            Self::Feature(invariant) => invariant.number(),
        }
    }

    #[must_use]
    pub fn label(self) -> String {
        match self {
            Self::I1 => "I1 acked-implies-visible".to_owned(),
            Self::I2 => "I2 deleted-implies-gone".to_owned(),
            Self::I3 => "I3 exactness".to_owned(),
            Self::I4 => "I4 durability-prefix".to_owned(),
            Self::I5 => "I5 filtered-results-honor-predicate".to_owned(),
            Self::I6 => "I6 accounting-conservation".to_owned(),
            Self::I7 => "I7 corruption-never-consumed".to_owned(),
            Self::I8 => "I8 lifecycle".to_owned(),
            Self::I9 => "I9 generation-monotonicity".to_owned(),
            Self::I10 => "I10 purge-proof".to_owned(),
            Self::I11 => "I11 revision-ordering".to_owned(),
            Self::I12 => "I12 responses-name-the-store-epoch".to_owned(),
            Self::I13 => "I13 diagnostics-never-lie".to_owned(),
            Self::I14 => "I14 alias-target-is-complete-and-single-epoch".to_owned(),
            Self::I54 => "I54 deadline-correctness".to_owned(),
            Self::I55 => "I55 cancellation-has-no-partial-results".to_owned(),
            Self::Feature(invariant) => format!("{} {}", invariant.key(), invariant.label()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Violation {
    pub invariant: Invariant,
    pub seed: u64,
    pub profile: FaultProfile,
    pub op_index: usize,
    pub detail: String,
}

impl Violation {
    #[must_use]
    pub fn report(&self) -> String {
        self.report_for(CampaignKind::Overall)
    }

    #[must_use]
    pub fn report_for(&self, campaign: CampaignKind) -> String {
        format!(
            "VIOLATION {} seed={} profile={} op={}: {} | reproduce: {}",
            self.invariant.label(),
            self.seed,
            self.profile.key(),
            self.op_index,
            self.detail,
            reproduction_for(campaign, self.seed, Some(self.profile))
        )
    }

    #[must_use]
    pub fn json(&self) -> String {
        self.json_for(CampaignKind::Overall)
    }

    #[must_use]
    pub fn json_for(&self, campaign: CampaignKind) -> String {
        format!(
            "{{\"invariant\":\"{}\",\"seed\":{},\"profile\":\"{}\",\"op\":{},\"detail\":\"{}\",\"reproduce\":\"{}\"}}",
            json_escape(&self.invariant.label()),
            self.seed,
            self.profile.key(),
            self.op_index,
            json_escape(&self.detail),
            json_escape(&reproduction_for(campaign, self.seed, Some(self.profile)))
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunOutcome {
    pub campaign: CampaignKind,
    pub seed: u64,
    pub profile: FaultProfile,
    pub profile_overridden: bool,
    pub operations: usize,
    pub faults_fired: usize,
    pub scheduled_faults_fired: usize,
    pub feature_faults_scheduled: usize,
    pub feature_faults_fired: usize,
    pub missing_feature_faults: Vec<&'static str>,
    pub graph_searches: usize,
    pub filtered_searches: usize,
    pub filtered_graph_searches: usize,
    pub predicate_searches: usize,
    pub hybrid_searches: usize,
    pub text_documents_ingested: usize,
    pub store_lexical_searches: usize,
    pub store_hybrid_searches: usize,
    pub phrase_searches: usize,
    pub prefix_searches: usize,
    pub fuzzy_searches: usize,
    pub phonetic_encodes: usize,
    pub snippets_built: usize,
    pub hybrid_sealed_vector_documents: usize,
    pub hybrid_lexical_documents: usize,
    pub epoch_preparations: usize,
    pub epoch_alias_switches: usize,
    pub epoch_rollbacks: usize,
    pub epoch_drops: usize,
    pub rejected_dropped_epoch_rollbacks: usize,
    pub coverage: CoverageRegistry,
    pub violations: Vec<Violation>,
    pub program_bytes: Vec<u8>,
    pub faults_bytes: Vec<u8>,
    pub violations_bytes: Vec<u8>,
    pub coverage_bytes: Vec<u8>,
    pub oracle_bytes: Vec<u8>,
    pub controls_bytes: Vec<u8>,
    pub receipts_bytes: Vec<u8>,
    pub mutations_bytes: Vec<u8>,
    pub episode_bytes: Vec<u8>,
    pub family_artifact_bytes: BTreeMap<String, Vec<u8>>,
    pub comparison_counts: BTreeMap<String, u64>,
    pub comparison_pass_counts: BTreeMap<String, u64>,
    pub comparison_outcome_counts: BTreeMap<String, ComparisonOutcomeCounts>,
    pub same_seed_clean_controls: u64,
    pub integrated_feature_fault_receipts: u64,
    pub expected_feature_fault_receipts: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ComparisonOutcomeCounts {
    pub equal: u64,
    pub refused: u64,
}

pub fn verify_comparison_outcome_counts(
    expected: &BTreeMap<String, ComparisonOutcomeCounts>,
    reported: &BTreeMap<String, ComparisonOutcomeCounts>,
) -> Result<(), String> {
    if expected == reported {
        return Ok(());
    }
    let mismatches = expected
        .keys()
        .chain(reported.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|invariant| expected.get(*invariant) != reported.get(*invariant))
        .map(|invariant| {
            format!(
                "{invariant}: expected={:?} reported={:?}",
                expected.get(invariant),
                reported.get(invariant)
            )
        })
        .collect::<Vec<_>>();
    Err(format!(
        "comparison outcome counts disagree: {}",
        mismatches.join(", ")
    ))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelfTestBug {
    DropAcknowledgedWrite,
    WrongDocument,
    LeakTombstone,
    MisreportGeneration,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Hit {
    doc_id: u32,
    revision: u64,
    score: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct LexicalHit {
    doc_id: u32,
    revision: u64,
    score: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct HybridHit {
    doc_id: u32,
    vector_squared_l2: Option<f64>,
    lexical_bm25: Option<f64>,
    fused_score: f64,
}

#[derive(Clone, Debug, PartialEq)]
struct HybridObservation {
    hits: Vec<HybridHit>,
    generation: u64,
    report_epoch: Option<EpochIdentity>,
}

#[derive(Clone, Debug, PartialEq)]
struct SearchObservation {
    hits: Vec<Hit>,
    generation: u64,
    epoch: Option<EpochIdentity>,
    graph_available: bool,
    graph_segments: usize,
    graph_rescored: usize,
    graph_pruned: usize,
    diagnostics_requested_k: usize,
    diagnostics_returned: usize,
    diagnostics_approximate: bool,
    diagnostics_exact_rescore: bool,
    diagnostics_budget_exhausted: bool,
    diagnostics_counters_match: bool,
    diagnostics_plan_matches_execution: bool,
    expected_exact_rescore: bool,
    expected_budget_exhausted: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct UnfilteredSegmentExecution {
    source: RowSource,
    tier: SegmentTier,
    live_rows: u64,
    row_count: u32,
}

fn observe_unfiltered_segments(store: &Store) -> Result<Vec<UnfilteredSegmentExecution>, String> {
    let snapshot = store.snapshot().map_err(|error| error.to_string())?;
    let mut segments = snapshot
        .segments()
        .iter()
        .map(|segment| {
            let tier = if segment.directory().iter().any(|entry| {
                entry.kind == zeppelin_embed::segment::layout::RegionKind::GraphNodeBlocks.id()
            }) {
                SegmentTier::SealedGraph
            } else {
                SegmentTier::SealedScan
            };
            Ok(UnfilteredSegmentExecution {
                source: RowSource::Sealed(segment.meta().id),
                tier,
                live_rows: segment
                    .alive()
                    .map_err(|error| error.to_string())?
                    .live_count(),
                row_count: segment.meta().row_count,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    segments.sort_unstable_by(|left, right| {
        right
            .row_count
            .cmp(&left.row_count)
            .then_with(|| left.source.cmp(&right.source))
    });
    Ok(segments)
}

fn unfiltered_plan_node_matches(plan: &SegmentPlan) -> bool {
    match (&plan.node, plan.branch) {
        (PlanNode::Scan { source, branch }, SegmentBranch::MaskedScan | SegmentBranch::Pruned) => {
            *source == plan.source && *branch == plan.branch
        }
        (
            PlanNode::Graph {
                source,
                fallback: None,
            },
            SegmentBranch::Graph,
        ) => *source == plan.source,
        _ => false,
    }
}

fn unfiltered_plan_matches_execution(
    plans: &[SegmentPlan],
    kind: SearchKind,
    segments: &[UnfilteredSegmentExecution],
    graph: GraphSearchStats,
) -> bool {
    let mut active_plans = 0_usize;
    let mut sealed_plans = Vec::new();
    let mut traversed = 0_usize;
    let mut pruned = 0_usize;

    for plan in plans {
        if plan.filter_mode != FilterMode::None
            || plan.fallback != PlanFallback::None
            || !unfiltered_plan_node_matches(plan)
        {
            return false;
        }
        match plan.source {
            RowSource::Active => {
                active_plans = active_plans.saturating_add(1);
                if active_plans > 1
                    || plan.tier != SegmentTier::ActiveScan
                    || plan.branch != SegmentBranch::MaskedScan
                    || plan.approximate
                    || plan.ef_requested.is_some()
                    || plan.ef_effective.is_some()
                {
                    return false;
                }
            }
            RowSource::Sealed(_) => {
                traversed =
                    traversed.saturating_add(usize::from(plan.branch == SegmentBranch::Graph));
                pruned = pruned.saturating_add(usize::from(plan.branch == SegmentBranch::Pruned));
                sealed_plans.push(plan);
            }
        }
    }

    if sealed_plans.len() != segments.len()
        || traversed != graph.segments_traversed
        || pruned != graph.segments_pruned_by_bound
    {
        return false;
    }

    sealed_plans
        .into_iter()
        .zip(segments)
        .all(|(plan, executed)| {
            let branch_matches_tier = matches!(
                (kind, executed.tier, plan.branch),
                (SearchKind::Scan, _, SegmentBranch::MaskedScan)
                    | (
                        SearchKind::Auto,
                        SegmentTier::SealedScan,
                        SegmentBranch::MaskedScan
                    )
                    | (
                        SearchKind::Auto | SearchKind::Graph,
                        SegmentTier::SealedGraph,
                        SegmentBranch::Graph | SegmentBranch::Pruned,
                    )
            );
            let graph_shape = match plan.branch {
                SegmentBranch::Graph => {
                    plan.approximate && plan.ef_requested.is_none() && plan.ef_effective.is_some()
                }
                SegmentBranch::MaskedScan | SegmentBranch::Pruned => {
                    !plan.approximate && plan.ef_requested.is_none() && plan.ef_effective.is_none()
                }
                _ => false,
            };
            plan.source == executed.source
                && plan.tier == executed.tier
                && plan.filter_cardinality == executed.live_rows
                && branch_matches_tier
                && graph_shape
        })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MutationAck {
    generation: u64,
    changed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct StatsObservation {
    resident_owned_bytes: u64,
    active_segment_bytes: u64,
    wal_bytes: u64,
    cache_bytes: u64,
    temporary_bytes: u64,
    query_pool_bytes: u64,
    mapped_bytes: u64,
    segment_bytes: u64,
    tombstone_bytes: u64,
    open_files: u64,
    active_queries: u64,
    active_snapshot_leases: u64,
}

#[derive(Clone, Copy, Debug)]
struct DocMutation {
    doc_id: u32,
    revision: u64,
    timestamp: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CrashDisposition {
    Unacknowledged,
    Visible,
    Gone,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CrashRecovery {
    generation: u64,
    disposition: CrashDisposition,
}

trait Engine {
    fn open(&mut self) -> Result<(), String>;
    fn ingest(&mut self, documents: &[DocMutation]) -> Result<MutationAck, String>;
    fn epoch_mismatch_probe(&mut self, document: DocMutation) -> Result<(), String>;
    fn prepare_epoch_b(&mut self, documents: &[DocMutation]) -> Result<MutationAck, String>;
    fn switch_epoch(&mut self, epoch: ModelEpoch) -> Result<MutationAck, String>;
    fn drop_epoch_a(&mut self) -> Result<MutationAck, String>;
    fn rollback_dropped_a_probe(&mut self) -> Result<(), String>;
    fn visible_epoch_ids(&mut self) -> Result<Vec<EpochId>, String>;
    fn delete(&mut self, doc_id: u32) -> Result<MutationAck, String>;
    fn seal(&mut self) -> Result<MutationAck, String>;
    fn drop_partition(&mut self, start: i64, end: i64) -> Result<MutationAck, String>;
    fn purge(&mut self, doc_id: u32) -> Result<MutationAck, String>;
    fn maintain(&mut self, bytes: u64) -> Result<MutationAck, String>;
    fn search(
        &mut self,
        query: &[f32],
        k: usize,
        kind: SearchKind,
        seed: u64,
    ) -> Result<SearchObservation, String>;
    fn filtered_search(
        &mut self,
        query: &[f32],
        k: usize,
        maximum_timestamp: i64,
    ) -> Result<SearchObservation, String>;
    fn predicate_search(
        &mut self,
        query: &[f32],
        k: usize,
        predicate: program::PredicateKind,
    ) -> Result<SearchObservation, String>;
    fn lexical_search(&mut self, query_slot: u8, k: usize) -> Result<Vec<LexicalHit>, String>;
    fn hybrid_search(&mut self, query_slot: u8, k: usize) -> Result<HybridObservation, String>;
    fn stats(&mut self) -> Result<StatsObservation, String>;
    fn close(&mut self) -> Result<(), String>;
    fn reopen(&mut self) -> Result<(), String>;
    fn crash_at_boundary(
        &mut self,
        mutation: DocMutation,
        boundary: program::CrashBoundary,
        campaign: CampaignKind,
        seed: u64,
        op_index: usize,
        profile: FaultProfile,
    ) -> Result<CrashRecovery, String>;
    fn generation(&mut self) -> Result<u64, String>;
    fn reset_query_cancelled(&mut self);
    fn query_cancelled(&self) -> bool;
}

struct RealEngine {
    directory: PathBuf,
    store: Option<Store>,
    vfs: Arc<fault_vfs::ScheduledVfs<fault_vfs::SimulatedCrashVfs<StdVfs>>>,
    manual_clock: Arc<ManualMonotonicClock>,
    clock: Arc<fault_vfs::ScheduledQueryClock<fault_vfs::SimulatedCrashVfs<StdVfs>>>,
    deadline_probe: Mutex<Option<Deadline>>,
    query_cancelled: bool,
    graphs_built: u64,
    open_epoch: ModelEpoch,
}

impl RealEngine {
    fn new(
        directory: PathBuf,
        vfs: Arc<fault_vfs::ScheduledVfs<fault_vfs::SimulatedCrashVfs<StdVfs>>>,
        clock: Arc<ManualMonotonicClock>,
    ) -> Self {
        let scheduled_clock = Arc::new(fault_vfs::ScheduledQueryClock::new(
            Arc::clone(&clock),
            Arc::clone(&vfs),
        ));
        Self {
            directory,
            store: None,
            vfs,
            manual_clock: clock,
            clock: scheduled_clock,
            deadline_probe: Mutex::new(None),
            query_cancelled: false,
            graphs_built: 0,
            open_epoch: ModelEpoch::A,
        }
    }

    fn without_faults(directory: PathBuf) -> Self {
        Self::new(
            directory,
            Arc::new(fault_vfs::simulated_scheduled(
                fault_vfs::FaultSchedule::default(),
            )),
            Arc::new(ManualMonotonicClock::new()),
        )
    }

    fn recover_from_simulated_crash(&mut self) -> Result<(), String> {
        self.store.take();
        self.vfs = Arc::new(self.vfs.restart_after_crash());
        self.open()
    }

    fn wal_dirent_is_durable(&self) -> Result<bool, String> {
        self.vfs
            .dirent_is_durable(&self.directory.join("wal.ze"))
            .map_err(|error| error.to_string())
    }

    fn store(&self) -> Result<&Store, String> {
        self.store
            .as_ref()
            .ok_or_else(|| "store is not open".to_owned())
    }

    fn arm_deadline_probe(&self) -> Result<(), String> {
        let deadline = Deadline::after_with_test_clock(Duration::from_secs(60), self.clock.clone())
            .map_err(|error| error.to_string())?;
        self.manual_clock.advance(Duration::from_secs(120));
        let mut armed = self
            .deadline_probe
            .lock()
            .map_err(|_| "deadline probe mutex was poisoned".to_owned())?;
        *armed = Some(deadline);
        Ok(())
    }

    fn query_control(&self) -> Result<QueryControl, String> {
        let deadline = self
            .deadline_probe
            .lock()
            .map_err(|_| "deadline probe mutex was poisoned".to_owned())?
            .take();
        if let Some(token) = self.vfs.cancel_token()? {
            return Ok(QueryControl::Cancel(token));
        }
        if let Some(deadline) = deadline {
            return Ok(QueryControl::Deadline(deadline));
        }
        let Some(event) = self.vfs.current_clock_event() else {
            return Ok(QueryControl::Cancel(CancelToken::new()));
        };
        let budget = event
            .deadline_budget_seconds
            .ok_or_else(|| format!("clock fault {} omitted its deadline budget", event.id))?;
        let deadline =
            Deadline::after_with_test_clock(Duration::from_secs(budget), self.clock.clone())
                .map(QueryControl::Deadline)
                .map_err(|error| error.to_string())?;
        self.clock.arm_query();
        Ok(deadline)
    }

    fn run_query<T>(
        &self,
        query: impl FnOnce(QueryControl) -> Result<T, String>,
    ) -> Result<T, String> {
        let control = self.query_control()?;
        let result = query(control);
        self.clock.finish_query()?;
        result
    }

    fn options(epoch: ModelEpoch) -> OpenOptions {
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_schema(adversarial_schema())
            .with_epoch(match epoch {
                ModelEpoch::A => declared_store_epoch(),
                ModelEpoch::B => epoch_b_store_epoch(),
            })
    }
}

fn require_second_opener_refusal(engine: &mut RealEngine) -> Result<(), String> {
    if engine.store.is_none() {
        return Err("first handle is not open before second-opener probe".to_owned());
    }
    match Store::open(&engine.directory, RealEngine::options(engine.open_epoch)) {
        Err(StoreError::StoreBusy { .. }) => {}
        Err(error) => {
            return Err(format!(
                "second opener returned {error} instead of StoreBusy"
            ));
        }
        Ok(second) => {
            return match second.close() {
                Ok(()) => Err("second opener acquired the writer lock".to_owned()),
                Err(error) => Err(format!(
                    "second opener acquired the writer lock; close failed: {error}"
                )),
            };
        }
    }
    engine
        .stats()
        .map(|_| ())
        .map_err(|error| format!("first handle failed after second-opener refusal: {error}"))
}

fn store_directory_entries(directory: &Path) -> Result<Vec<PathBuf>, String> {
    let mut entries = std::fs::read_dir(directory)
        .map_err(|error| error.to_string())?
        .map(|entry| {
            entry
                .map(|entry| PathBuf::from(entry.file_name()))
                .map_err(|error| error.to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort();
    Ok(entries)
}

pub(crate) fn second_opener_in_process_for_test() -> Result<(), String> {
    let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
    let mut engine = RealEngine::without_faults(directory.path().to_path_buf());
    engine.open()?;
    let result = require_second_opener_refusal(&mut engine);
    let close = engine.close();
    result.and(close)
}

pub(crate) fn second_opener_artifacts_for_test() -> Result<(Vec<PathBuf>, Vec<PathBuf>), String> {
    let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
    let mut engine = RealEngine::without_faults(directory.path().to_path_buf());
    engine.open()?;
    let before = store_directory_entries(directory.path())?;
    require_second_opener_refusal(&mut engine)?;
    let after = store_directory_entries(directory.path())?;
    engine.close()?;
    Ok((before, after))
}

pub(crate) fn second_opener_without_first_handle_for_test()
-> Result<(Vec<PathBuf>, Vec<PathBuf>, String), String> {
    let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
    let mut engine = RealEngine::without_faults(directory.path().to_path_buf());
    let before = store_directory_entries(directory.path())?;
    let error = require_second_opener_refusal(&mut engine)
        .expect_err("second-opener probe accepted an absent first handle");
    let after = store_directory_entries(directory.path())?;
    Ok((before, after, error))
}

struct BusyChildProcess {
    child: Option<Child>,
    release: Option<UnixStream>,
}

impl BusyChildProcess {
    fn finish(mut self) -> Result<(), String> {
        let mut release = self
            .release
            .take()
            .ok_or_else(|| "busy child release pipe is absent".to_owned())?;
        release
            .write_all(&[1])
            .map_err(|error| format!("release busy child: {error}"))?;
        drop(release);
        let child = self
            .child
            .take()
            .ok_or_else(|| "busy child process is absent".to_owned())?;
        let output = child
            .wait_with_output()
            .map_err(|error| format!("wait for busy child: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "busy child failed: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        Ok(())
    }
}

impl Drop for BusyChildProcess {
    fn drop(&mut self) {
        self.release.take();
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(target_os = "macos")]
fn descriptor_path(raw_fd: i32) -> Option<PathBuf> {
    unsafe extern "C" {
        fn fcntl(fd: i32, command: i32, ...) -> i32;
    }
    const F_GETPATH: i32 = 50;
    const MAX_PATH_BYTES: usize = 1_024;
    let mut path = [0_i8; MAX_PATH_BYTES];
    let status = unsafe {
        // SAFETY: F_GETPATH writes a NUL-terminated path into this fixed-size
        // buffer and does not retain the pointer.
        fcntl(raw_fd, F_GETPATH, path.as_mut_ptr())
    };
    if status == -1 {
        return None;
    }
    let path = unsafe {
        // SAFETY: successful F_GETPATH initialized a NUL-terminated string.
        std::ffi::CStr::from_ptr(path.as_ptr())
    };
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(path.to_bytes())))
}

#[cfg(target_os = "linux")]
fn descriptor_path(raw_fd: i32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/self/fd/{raw_fd}")).ok()
}

fn store_lock_descriptor(directory: &Path) -> Result<i32, String> {
    let lock_path = std::fs::canonicalize(directory.join("writer.lock"))
        .map_err(|error| format!("resolve writer lock: {error}"))?;
    for raw_fd in 3..256 {
        if descriptor_path(raw_fd).as_deref() == Some(lock_path.as_path()) {
            return Ok(raw_fd);
        }
    }
    Err("open writer-lock descriptor was not found".to_owned())
}

fn spawn_busy_child(engine: &RealEngine) -> Result<BusyChildProcess, String> {
    let lock_fd = store_lock_descriptor(&engine.directory)?;
    let (read, release) =
        UnixStream::pair().map_err(|error| format!("create busy child pipe: {error}"))?;
    let read: OwnedFd = read.into();
    const CHILD_LOCK_FD: i32 = 198;
    unsafe extern "C" {
        fn dup2(source: i32, destination: i32) -> i32;
    }
    let mut command = Command::new(std::env::current_exe().map_err(|error| error.to_string())?);
    command
        .args(["busy_child", "--ignored", "--exact", "--nocapture"])
        .env("ZE_ADV_BUSY_CHILD_LOCK_FD", CHILD_LOCK_FD.to_string())
        .stdin(Stdio::from(read))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    unsafe {
        // SAFETY: this closure performs only the async-signal-safe `dup2`
        // between fork and exec. The Store keeps `lock_fd` live through spawn.
        command.pre_exec(move || {
            if dup2(lock_fd, CHILD_LOCK_FD) == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let child = command
        .spawn()
        .map_err(|error| format!("spawn busy child: {error}"))?;
    Ok(BusyChildProcess {
        child: Some(child),
        release: Some(release),
    })
}

pub(crate) fn busy_child_round_trip_for_test() -> Result<(Vec<PathBuf>, Vec<PathBuf>), String> {
    let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
    let mut engine = RealEngine::without_faults(directory.path().to_path_buf());
    engine.open()?;
    let before = store_directory_entries(directory.path())?;
    spawn_busy_child(&engine)?.finish()?;
    let after = store_directory_entries(directory.path())?;
    engine.close()?;
    Ok((before, after))
}

pub fn busy_child_from_env() -> Result<(), String> {
    let lock_fd = std::env::var("ZE_ADV_BUSY_CHILD_LOCK_FD")
        .map_err(|_| "ZE_ADV_BUSY_CHILD_LOCK_FD is absent".to_owned())?
        .parse::<i32>()
        .map_err(|_| "ZE_ADV_BUSY_CHILD_LOCK_FD is not an integer".to_owned())?;
    let lock = unsafe {
        // SAFETY: the parent maps its live Store lock descriptor to this
        // numeric descriptor before exec and keeps no child-side Rust owner.
        BorrowedFd::borrow_raw(lock_fd)
    };
    let _lock_guard = lock
        .try_clone_to_owned()
        .map_err(|_| "inherited writer-lock descriptor is closed".to_owned())?;
    let mut release = [0_u8; 1];
    std::io::stdin()
        .read_exact(&mut release)
        .map_err(|error| format!("wait on busy child pipe: {error}"))?;
    Ok(())
}

fn reopen_after_spawn(engine: &mut RealEngine) -> Result<(), String> {
    if let Some(store) = engine.store.take() {
        store.close().map_err(|error| error.to_string())?;
    }
    match Store::open_with_test_dependencies(
        &engine.directory,
        RealEngine::options(engine.open_epoch),
        StoreTestDependencies::new(engine.vfs.clone(), engine.clock.clone()),
    ) {
        Ok(store) => {
            engine.store = Some(store);
            Ok(())
        }
        Err(StoreError::StoreBusy { .. }) => Err(
            "SpawnInFlight: Reopen returned StoreBusy on first attempt; lock leaked across spawn"
                .to_owned(),
        ),
        Err(error) => Err(format!("Reopen returned a non-busy error: {error}")),
    }
}

pub(crate) fn spawn_in_flight_reopen_for_test() -> Result<(), String> {
    let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
    let mut engine = RealEngine::without_faults(directory.path().to_path_buf());
    engine.open()?;
    let child = spawn_busy_child(&engine)?;
    let first = reopen_after_spawn(&mut engine);
    child.finish()?;
    first?;
    engine.stats()?;
    engine.close()?;
    Ok(())
}

fn run_cancel_aware_query<T, E: std::fmt::Display>(
    control: QueryControl,
    run: impl Fn(QueryControl) -> Result<T, E>,
    is_typed_cancelled: impl Fn(&E) -> bool,
) -> Result<(T, bool), String> {
    match run(control) {
        Ok(value) => Ok((value, false)),
        Err(error) if is_typed_cancelled(&error) => run(QueryControl::Cancel(CancelToken::new()))
            .map(|value| (value, true))
            .map_err(|retry_error| {
                format!("follow-up uncontrolled query after cancellation failed: {retry_error}")
            }),
        Err(error) => Err(error.to_string()),
    }
}

fn run_cancel_aware_query_with_follow_up<T, E: std::fmt::Display>(
    control: QueryControl,
    run: impl FnOnce(QueryControl) -> Result<T, E>,
    follow_up: impl FnOnce(QueryControl) -> Result<T, E>,
    is_typed_cancelled: impl Fn(&E) -> bool,
) -> Result<(T, bool), String> {
    match run(control) {
        Ok(value) => Ok((value, false)),
        Err(error) if is_typed_cancelled(&error) => {
            follow_up(QueryControl::Cancel(CancelToken::new()))
                .map(|value| (value, true))
                .map_err(|retry_error| {
                    format!("follow-up uncontrolled query after cancellation failed: {retry_error}")
                })
        }
        Err(error) => Err(error.to_string()),
    }
}

impl Engine for RealEngine {
    fn open(&mut self) -> Result<(), String> {
        if self.store.is_some() {
            return Err("store is already open".to_owned());
        }
        self.store = Some(
            Store::open_with_test_dependencies(
                &self.directory,
                Self::options(self.open_epoch),
                StoreTestDependencies::new(self.vfs.clone(), self.clock.clone()),
            )
            .map_err(|error| error.to_string())?,
        );
        Ok(())
    }

    fn ingest(&mut self, documents: &[DocMutation]) -> Result<MutationAck, String> {
        let batch = documents
            .iter()
            .map(|document| {
                IngestDocument::new(
                    DocumentVersion::new(
                        DocId::new(u128::from(document.doc_id)),
                        Revision::new(document.revision),
                    ),
                    program::vector(document.doc_id, document.revision).to_vec(),
                )
                .with_timestamp(document.timestamp)
                .with_metadata(program::sentinel(document.doc_id))
                .with_text(program::lexical_text(document.doc_id, document.revision))
                .with_columns(adversarial_columns(document.doc_id))
            })
            .collect::<Vec<_>>();
        let ack = self
            .store()?
            .ingest(IngestBatch::new(batch).with_epoch(declared_identity()))
            .map_err(|error| error.to_string())?;
        Ok(MutationAck {
            generation: ack.generation(),
            changed: true,
        })
    }

    fn epoch_mismatch_probe(&mut self, document: DocMutation) -> Result<(), String> {
        let before = self.generation()?;
        let batch = IngestBatch::new(vec![
            IngestDocument::new(
                DocumentVersion::new(
                    DocId::new(u128::from(document.doc_id)),
                    Revision::new(document.revision),
                ),
                program::vector(document.doc_id, document.revision).to_vec(),
            )
            .with_timestamp(document.timestamp)
            .with_metadata(program::sentinel(document.doc_id))
            .with_text(program::lexical_text(document.doc_id, document.revision))
            .with_columns(adversarial_columns(document.doc_id)),
        ])
        .with_epoch(conflicting_identity());
        match self.store()?.ingest(batch) {
            Err(IngestError::EpochMismatch(_)) => {}
            Err(error) => return Err(format!("epoch probe returned wrong typed error: {error}")),
            Ok(_) => return Err("epoch probe accepted a conflicting identity".to_owned()),
        }
        let analyzer_probe = OpenOptions::read_only()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_schema(adversarial_schema())
            .with_epoch(declared_store_epoch())
            .with_tokenizer(TokenizerConfig::code());
        match Store::open_with_test_dependencies(
            &self.directory,
            analyzer_probe,
            StoreTestDependencies::new(self.vfs.clone(), self.clock.clone()),
        ) {
            Err(StoreError::EpochMismatch(_)) => {}
            Err(error) => {
                return Err(format!(
                    "analyzer epoch probe returned wrong typed error: {error}"
                ));
            }
            Ok(store) => {
                store.close().map_err(|error| error.to_string())?;
                return Err("analyzer epoch probe accepted a conflicting tokenizer".to_owned());
            }
        }
        let after = self.generation()?;
        if after != before {
            return Err(format!(
                "epoch probe changed generation from {before} to {after}"
            ));
        }
        Ok(())
    }

    fn prepare_epoch_b(&mut self, documents: &[DocMutation]) -> Result<MutationAck, String> {
        if documents.is_empty() {
            return Err("cannot prepare epoch B without live documents".to_owned());
        }
        if let Some(store) = self.store.take() {
            store.close().map_err(|error| error.to_string())?;
        }

        let manifest_path = self.directory.join("manifest.ze");
        let mut manifest = load_manifest(self.vfs.as_ref(), &manifest_path, u64::MAX)
            .map_err(|error| error.to_string())?;
        let rows = documents.len();
        let mut vectors = Vec::with_capacity(rows.saturating_mul(program::DIMENSIONS));
        let mut codes = vec![0_u8; rows.saturating_mul(program::DIMENSIONS.div_ceil(2))];
        let mut factors = Vec::<Bit4Factors>::with_capacity(rows);
        let mut columns = ColumnStoreBuilder::new(manifest.schema.clone());
        let mut doc_ids = Vec::with_capacity(rows);
        let mut revisions = Vec::with_capacity(rows);
        for (row, document) in documents.iter().enumerate() {
            let vector = program::vector(document.doc_id, document.revision);
            factors.push(
                quantize_bit4(
                    &vector,
                    &mut codes[row * program::DIMENSIONS.div_ceil(2)
                        ..(row + 1) * program::DIMENSIONS.div_ceil(2)],
                )
                .map_err(|error| error.to_string())?,
            );
            vectors.extend_from_slice(&vector);
            columns
                .push_row(document.timestamp, &[])
                .map_err(|error| error.to_string())?;
            doc_ids.push(DocId::new(u128::from(document.doc_id)));
            revisions.push(Revision::new(document.revision));
        }
        let columns = columns.finish().map_err(|error| error.to_string())?;
        let alive = AliveSet::new(u32::try_from(rows).map_err(|_| "too many epoch rows")?);
        let segment_id = SegmentId::new(
            0x0021_e000_0000_u64.saturating_add(manifest.generation),
            [0xbe; 10],
        );
        let policy = DurabilityPolicy::new(DurabilityMode::Durable, CommitTier::Durable)
            .map_err(|error| error.to_string())?;
        let mut segment = write_segment_with_documents(
            self.vfs.as_ref(),
            &self.directory,
            SegmentBuild {
                id: segment_id,
                scheme: 4,
                dims: program::DIMENSIONS as u32,
                codes: &codes,
                factors: SegmentFactors::Bit4(&factors),
                rescore: &vectors,
                columns: &columns,
                alive: &alive,
            },
            SegmentDocumentVersions {
                doc_ids: &doc_ids,
                revisions: &revisions,
            },
            policy,
        )
        .map_err(|error| error.to_string())?;
        let epoch_b = epoch_b_store_epoch();
        segment.epoch_id = Some(epoch_b.identity().embedding);
        manifest.segments.push(segment);
        if !manifest
            .epochs
            .iter()
            .any(|epoch| epoch.id == epoch_b.identity().embedding)
        {
            manifest.epochs.push(EpochMeta::from(&epoch_b));
        }
        manifest.generation = manifest
            .generation
            .checked_add(1)
            .ok_or_else(|| "manifest generation overflow while preparing epoch B".to_owned())?;
        commit_manifest(self.vfs.as_ref(), &self.directory, &manifest, policy)
            .map_err(|error| error.to_string())?;
        self.open_epoch = ModelEpoch::A;
        self.open()?;
        Ok(MutationAck {
            generation: manifest.generation,
            changed: true,
        })
    }

    fn switch_epoch(&mut self, epoch: ModelEpoch) -> Result<MutationAck, String> {
        let report = self
            .store()?
            .switch_epoch_alias(identity_for(epoch))
            .map_err(|error| error.to_string())?;
        self.open_epoch = epoch;
        Ok(MutationAck {
            generation: report.generation(),
            changed: report.manifest_committed(),
        })
    }

    fn drop_epoch_a(&mut self) -> Result<MutationAck, String> {
        let report = self
            .store()?
            .drop_epoch(declared_identity().embedding)
            .map_err(|error| error.to_string())?;
        Ok(MutationAck {
            generation: report.generation(),
            changed: true,
        })
    }

    fn rollback_dropped_a_probe(&mut self) -> Result<(), String> {
        match self.store()?.switch_epoch_alias(declared_identity()) {
            Err(EpochTransitionError::EpochUnavailable { target })
                if target == declared_identity() =>
            {
                Ok(())
            }
            Err(error) => Err(format!(
                "rollback probe returned wrong typed error: {error}"
            )),
            Ok(_) => Err("rollback probe restored a dropped epoch".to_owned()),
        }
    }

    fn visible_epoch_ids(&mut self) -> Result<Vec<EpochId>, String> {
        self.store()?
            .snapshot()
            .map_err(|error| error.to_string())
            .map(|snapshot| {
                snapshot
                    .segments()
                    .iter()
                    .filter_map(|segment| segment.meta().epoch_id)
                    .collect()
            })
    }

    fn delete(&mut self, doc_id: u32) -> Result<MutationAck, String> {
        let ack = self
            .store()?
            .delete(DeleteBatch::new(vec![DocId::new(u128::from(doc_id))]))
            .map_err(|error| error.to_string())?;
        Ok(MutationAck {
            generation: ack.generation(),
            changed: true,
        })
    }

    fn seal(&mut self) -> Result<MutationAck, String> {
        let store = self.store()?;
        let changed = store
            .stats()
            .map_err(|error| error.to_string())?
            .active_row_count
            > 0;
        let generation = store
            .seal_with_cancel(&CancelToken::new())
            .map_err(|error| error.to_string())?;
        Ok(MutationAck {
            generation,
            changed,
        })
    }

    fn drop_partition(&mut self, start: i64, end: i64) -> Result<MutationAck, String> {
        let report = self
            .store()?
            .drop_partition(start..end)
            .map_err(|error| error.to_string())?;
        Ok(MutationAck {
            generation: report.generation(),
            changed: !report.is_no_op(),
        })
    }

    fn purge(&mut self, doc_id: u32) -> Result<MutationAck, String> {
        let token = self
            .store()?
            .purge(&[DocId::new(u128::from(doc_id))])
            .map_err(|error| error.to_string())?;
        let report = self
            .store()?
            .await_physical_purge(token)
            .map_err(|error| error.to_string())?;
        Ok(MutationAck {
            generation: report.generation(),
            changed: !report.is_no_op(),
        })
    }

    fn maintain(&mut self, bytes: u64) -> Result<MutationAck, String> {
        let before = self.generation()?;
        let report = self.store()?.maintain_with_test_thresholds(
            MaintenanceBudget {
                wall_time: Duration::from_secs(120),
                bytes,
            },
            TierThresholds {
                graph_min_rows: program::GRAPH_ROWS,
            },
        );
        if let MaintenanceStatus::Failed(error) = &report.status {
            return Err(error.to_string());
        }
        self.graphs_built = self.graphs_built.saturating_add(report.graphs_built);
        let generation = self.generation()?;
        Ok(MutationAck {
            generation,
            changed: generation > before,
        })
    }

    fn search(
        &mut self,
        query: &[f32],
        k: usize,
        kind: SearchKind,
        seed: u64,
    ) -> Result<SearchObservation, String> {
        let execution_segments = observe_unfiltered_segments(self.store()?)?;
        let graph_available = self
            .store()?
            .snapshot()
            .map_err(|error| error.to_string())?
            .segments()
            .iter()
            .any(|segment| {
                segment.directory().iter().any(|entry| {
                    entry.kind == zeppelin_embed::segment::layout::RegionKind::GraphNodeBlocks.id()
                })
            });
        let cancel_after_hops = self.vfs.cancel_after_hops()?;
        let clean_tier = match kind {
            SearchKind::Scan => SearchTier::Scan,
            SearchKind::Auto => SearchTier::Auto,
            SearchKind::Graph => SearchTier::Graph(
                GraphSearchOptions::new(GraphSearchProfile::SiftClass).with_seed(seed),
            ),
        };
        let tier = match clean_tier {
            SearchTier::Graph(options) => {
                let options =
                    cancel_after_hops.map_or(options, |hops| options.with_cancel_after_hops(hops));
                SearchTier::Graph(options)
            }
            SearchTier::Auto | SearchTier::Exact | SearchTier::Scan => clean_tier,
        };
        let (outcome, cancelled) = self.run_query(|control| {
            let store = self.store()?;
            let run = |tier, control| {
                store.search(
                    SearchRequest::new(query),
                    k,
                    SearchOptions::new(ScanOptions {
                        thread_budget: THREAD_BUDGET,
                    })
                    .with_tier(tier),
                    control,
                )
            };
            run_cancel_aware_query_with_follow_up(
                control,
                |control| run(tier, control),
                |control| run(clean_tier, control),
                |error| matches!(error, QueryError::Cancelled { partial: false }),
            )
        })?;
        if cancelled && cancel_after_hops.is_some() {
            self.vfs.record_graph_cancel()?;
        }
        self.query_cancelled |= cancelled;
        let hits = outcome
            .candidates
            .iter()
            .map(|candidate| {
                let version = candidate
                    .document()
                    .ok_or_else(|| "search returned a row without document identity".to_owned())?;
                let doc_id = u32::try_from(version.doc_id().get())
                    .map_err(|_| "document id exceeds adversarial vocabulary".to_owned())?;
                Ok(Hit {
                    doc_id,
                    revision: version.revision().get(),
                    score: candidate.score(),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let expected_exact_rescore = hits.is_empty()
            || matches!(kind, SearchKind::Graph)
            || (matches!(kind, SearchKind::Auto) && graph_available);
        Ok(SearchObservation {
            hits,
            generation: outcome.generation,
            epoch: outcome.epoch,
            graph_available,
            graph_segments: outcome.graph_stats.segments_traversed,
            graph_rescored: outcome.graph_stats.candidates_rescored,
            graph_pruned: outcome.graph_stats.segments_pruned_by_bound,
            diagnostics_requested_k: outcome.diagnostics.requested_k,
            diagnostics_returned: outcome.diagnostics.returned,
            diagnostics_approximate: outcome.diagnostics.approximate,
            diagnostics_exact_rescore: outcome.diagnostics.exact_rescore,
            diagnostics_budget_exhausted: outcome.diagnostics.budget_exhausted,
            diagnostics_counters_match: outcome.diagnostics.counters.scan == outcome.stats
                && outcome.diagnostics.counters.graph == outcome.graph_stats,
            diagnostics_plan_matches_execution: unfiltered_plan_matches_execution(
                &outcome.diagnostics.plan,
                kind,
                &execution_segments,
                outcome.graph_stats,
            ),
            expected_exact_rescore,
            expected_budget_exhausted: false,
        })
    }

    fn filtered_search(
        &mut self,
        query: &[f32],
        k: usize,
        maximum_timestamp: i64,
    ) -> Result<SearchObservation, String> {
        let predicate = Predicate::Range(RangePredicate {
            column: TIMESTAMP_COLUMN,
            lower: None,
            upper: Some(RangeBound::inclusive(PredicateValue::I64(
                maximum_timestamp,
            ))),
        });
        let (outcome, cancelled) = self.run_query(|control| {
            let store = self.store()?;
            run_cancel_aware_query_with_follow_up(
                control,
                |control| {
                    store.search_filtered(
                        SearchRequest::new(query),
                        &predicate,
                        k,
                        SearchOptions::new(ScanOptions {
                            thread_budget: THREAD_BUDGET,
                        }),
                        control,
                    )
                },
                |control| {
                    store.search_filtered(
                        SearchRequest::new(query),
                        &predicate,
                        k,
                        SearchOptions::new(ScanOptions {
                            thread_budget: THREAD_BUDGET,
                        }),
                        control,
                    )
                },
                |error| {
                    matches!(
                        error,
                        FilteredSearchError::Query(QueryError::Cancelled { partial: false })
                    )
                },
            )
        })?;
        self.query_cancelled |= cancelled;
        let graph_available = outcome
            .plans
            .iter()
            .any(|plan| plan.tier == SegmentTier::SealedGraph);
        let filtered_graph_segments = outcome
            .plans
            .iter()
            .filter(|plan| plan.branch == SegmentBranch::FilteredGraph)
            .count();
        let hits = outcome
            .candidates
            .iter()
            .map(|candidate| {
                let version = candidate
                    .document()
                    .ok_or_else(|| "filtered search returned a row without identity".to_owned())?;
                Ok(Hit {
                    doc_id: u32::try_from(version.doc_id().get())
                        .map_err(|_| "filtered document id exceeds vocabulary".to_owned())?,
                    revision: version.revision().get(),
                    score: candidate.score(),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let expected_exact_rescore = hits.is_empty() || graph_available;
        let expected_budget_exhausted = outcome.plans.iter().any(|plan| {
            matches!(
                plan.fallback,
                PlanFallback::VisitedBudget | PlanFallback::CandidateShortfall
            )
        });
        Ok(SearchObservation {
            hits,
            generation: outcome.generation,
            epoch: None,
            graph_available,
            graph_segments: filtered_graph_segments,
            graph_rescored: 0,
            graph_pruned: 0,
            diagnostics_requested_k: outcome.diagnostics.requested_k,
            diagnostics_returned: outcome.diagnostics.returned,
            diagnostics_approximate: outcome.diagnostics.approximate,
            diagnostics_exact_rescore: outcome.diagnostics.exact_rescore,
            diagnostics_budget_exhausted: outcome.diagnostics.budget_exhausted,
            diagnostics_counters_match: outcome.diagnostics.counters.scan == outcome.stats,
            diagnostics_plan_matches_execution: outcome.diagnostics.plan == outcome.plans,
            expected_exact_rescore,
            expected_budget_exhausted,
        })
    }

    fn predicate_search(
        &mut self,
        query: &[f32],
        k: usize,
        predicate: program::PredicateKind,
    ) -> Result<SearchObservation, String> {
        let predicate = adversarial_predicate(predicate);
        let (outcome, cancelled) = self.run_query(|control| {
            let store = self.store()?;
            run_cancel_aware_query_with_follow_up(
                control,
                |control| {
                    store.search_filtered(
                        SearchRequest::new(query),
                        &predicate,
                        k,
                        SearchOptions::new(ScanOptions {
                            thread_budget: THREAD_BUDGET,
                        })
                        .with_tier(SearchTier::Exact),
                        control,
                    )
                },
                |control| {
                    store.search_filtered(
                        SearchRequest::new(query),
                        &predicate,
                        k,
                        SearchOptions::new(ScanOptions {
                            thread_budget: THREAD_BUDGET,
                        })
                        .with_tier(SearchTier::Exact),
                        control,
                    )
                },
                |error| {
                    matches!(
                        error,
                        FilteredSearchError::Query(QueryError::Cancelled { partial: false })
                    )
                },
            )
        })?;
        self.query_cancelled |= cancelled;
        let hits = outcome
            .candidates
            .iter()
            .map(|candidate| {
                let version = candidate
                    .document()
                    .ok_or_else(|| "predicate search returned a row without identity".to_owned())?;
                Ok(Hit {
                    doc_id: u32::try_from(version.doc_id().get())
                        .map_err(|_| "predicate document id exceeds vocabulary".to_owned())?,
                    revision: version.revision().get(),
                    score: candidate.score(),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(SearchObservation {
            hits,
            generation: outcome.generation,
            epoch: None,
            graph_available: false,
            graph_segments: 0,
            graph_rescored: 0,
            graph_pruned: 0,
            diagnostics_requested_k: outcome.diagnostics.requested_k,
            diagnostics_returned: outcome.diagnostics.returned,
            diagnostics_approximate: outcome.diagnostics.approximate,
            diagnostics_exact_rescore: outcome.diagnostics.exact_rescore,
            diagnostics_budget_exhausted: outcome.diagnostics.budget_exhausted,
            diagnostics_counters_match: outcome.diagnostics.counters.scan == outcome.stats,
            diagnostics_plan_matches_execution: outcome.diagnostics.plan == outcome.plans,
            expected_exact_rescore: true,
            expected_budget_exhausted: false,
        })
    }

    fn lexical_search(&mut self, query_slot: u8, k: usize) -> Result<Vec<LexicalHit>, String> {
        let query = TermQuery::flat(
            vec![program::lexical_query(query_slot).to_vec()],
            &[DEFAULT_FIELD],
        );
        let (outcome, cancelled) = self.run_query(|control| {
            let store = self.store()?;
            run_cancel_aware_query_with_follow_up(
                control,
                |control| store.search_lexical(&query, k, control),
                |control| store.search_lexical(&query, k, control),
                |error| {
                    matches!(
                        error,
                        StoreLexicalError::Query(QueryError::Cancelled { partial: false })
                            | StoreLexicalError::Query(QueryError::Scan(ScanError::Cancelled {
                                partial: false
                            }))
                    )
                },
            )
        })?;
        self.query_cancelled |= cancelled;
        outcome
            .candidates
            .into_iter()
            .map(|candidate| {
                Ok(LexicalHit {
                    doc_id: u32::try_from(candidate.document.doc_id().get())
                        .map_err(|_| "lexical document id exceeds vocabulary".to_owned())?,
                    revision: candidate.document.revision().get(),
                    score: candidate.score,
                })
            })
            .collect()
    }

    fn hybrid_search(&mut self, query_slot: u8, k: usize) -> Result<HybridObservation, String> {
        let vector = program::query(query_slot);
        let lexical = TermQuery::flat(
            vec![program::lexical_query(query_slot).to_vec()],
            &[DEFAULT_FIELD],
        );
        let (outcome, cancelled) = self.run_query(|control| {
            let store = self.store()?;
            run_cancel_aware_query_with_follow_up(
                control,
                |control| {
                    store.search_hybrid(
                        SearchRequest::new(&vector),
                        &lexical,
                        &HybridQuery::new(k).with_epoch(declared_identity()),
                        SearchOptions::new(ScanOptions {
                            thread_budget: THREAD_BUDGET,
                        }),
                        control,
                    )
                },
                |control| {
                    store.search_hybrid(
                        SearchRequest::new(&vector),
                        &lexical,
                        &HybridQuery::new(k).with_epoch(declared_identity()),
                        SearchOptions::new(ScanOptions {
                            thread_budget: THREAD_BUDGET,
                        }),
                        control,
                    )
                },
                |error| matches!(error, FusionError::Cancelled { partial: false }),
            )
        })?;
        self.query_cancelled |= cancelled;
        let report_epoch = outcome
            .diagnostics
            .fusion
            .as_ref()
            .and_then(|report| report.epoch);
        let hits = outcome
            .hits
            .into_iter()
            .map(|hit| {
                Ok(HybridHit {
                    doc_id: u32::try_from(hit.key.get())
                        .map_err(|_| "hybrid document id exceeds vocabulary".to_owned())?,
                    vector_squared_l2: hit.vector_squared_l2,
                    lexical_bm25: hit.lexical_bm25,
                    fused_score: hit.fused_score,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(HybridObservation {
            hits,
            generation: outcome.generation,
            report_epoch,
        })
    }

    fn stats(&mut self) -> Result<StatsObservation, String> {
        let stats = self.store()?.stats().map_err(|error| error.to_string())?;
        Ok(StatsObservation {
            resident_owned_bytes: stats.resident_owned_bytes,
            active_segment_bytes: stats.active_segment_bytes,
            wal_bytes: stats.wal_bytes,
            cache_bytes: stats.cache_bytes,
            temporary_bytes: stats.temporary_bytes,
            query_pool_bytes: stats.query_pool_bytes,
            mapped_bytes: stats.mapped_bytes,
            segment_bytes: stats.segment_bytes,
            tombstone_bytes: stats.tombstone_bytes,
            open_files: stats.open_files,
            active_queries: stats.active_queries,
            active_snapshot_leases: stats.active_snapshot_leases,
        })
    }

    fn close(&mut self) -> Result<(), String> {
        self.store()?.close().map_err(|error| error.to_string())
    }

    fn reopen(&mut self) -> Result<(), String> {
        if let Some(store) = self.store.take() {
            store.close().map_err(|error| error.to_string())?;
        }
        self.open()
    }

    fn crash_at_boundary(
        &mut self,
        mutation: DocMutation,
        boundary: program::CrashBoundary,
        campaign: CampaignKind,
        seed: u64,
        op_index: usize,
        profile: FaultProfile,
    ) -> Result<CrashRecovery, String> {
        // Parent shutdown is harness plumbing; the child rebuilds and runs the plan.
        self.vfs.set_operation(usize::MAX);
        if let Some(store) = self.store.take() {
            store.close().map_err(|error| error.to_string())?;
        }
        let marker = self.directory.join(".adversarial-crash-ack");
        match std::fs::remove_file(&marker) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("remove stale crash marker: {error}")),
        }
        let output = Command::new(std::env::current_exe().map_err(|error| error.to_string())?)
            .args(["crash_child", "--ignored", "--exact", "--nocapture"])
            .env("ZE_ADV_CRASH_CHILD_PATH", &self.directory)
            .env("ZE_ADV_CRASH_CHILD_MARKER", &marker)
            .env("ZE_ADV_CRASH_CHILD_DOC", mutation.doc_id.to_string())
            .env("ZE_ADV_CRASH_CHILD_REV", mutation.revision.to_string())
            .env("ZE_ADV_CRASH_CHILD_TS", mutation.timestamp.to_string())
            .env("ZE_ADV_CRASH_BOUNDARY", boundary.key())
            .env("ZE_ADV_CRASH_CHILD_OP", op_index.to_string())
            .env("ZE_ADV_SEED", seed.to_string())
            .env("ZE_ADV_CAMPAIGN", campaign.key())
            .env("ZE_ADV_CRASH_CHILD_PROFILE", profile.key())
            .output()
            .map_err(|error| format!("spawn crash child: {error}"))?;
        self.vfs
            .import_fired_log(&self.directory.join("faults.jsonl"))
            .map_err(|error| format!("read crash child fault outcomes: {error}"))?;
        if output.status.success() {
            return Err("crash child exited successfully instead of crashing".to_owned());
        }
        if output.status.code().is_some() {
            return Err(format!(
                "crash child failed without a process abort: status={} stderr={}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        // The child changed the directory outside the parent's simulator.
        // Rebase its durability baseline before any later scheduled crash.
        self.vfs = Arc::new(self.vfs.restart_after_crash());
        self.open()?;
        let reopened_generation = self.generation()?;
        let marker_value = match std::fs::read_to_string(&marker) {
            Ok(value) => Some(value),
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && boundary == program::CrashBoundary::MidWalGroup =>
            {
                None
            }
            Err(error) => {
                return Err(format!(
                    "crash child did not persist its boundary acknowledgement: {error}"
                ));
            }
        };
        let Some(marker_value) = marker_value else {
            let survived = self
                .search(
                    &program::vector(mutation.doc_id, mutation.revision),
                    1_024,
                    SearchKind::Scan,
                    0,
                )?
                .hits
                .iter()
                .any(|hit| hit.doc_id == mutation.doc_id && hit.revision == mutation.revision);
            return Ok(CrashRecovery {
                generation: reopened_generation,
                disposition: if survived {
                    CrashDisposition::Visible
                } else {
                    CrashDisposition::Unacknowledged
                },
            });
        };
        std::fs::remove_file(&marker).map_err(|error| format!("remove crash marker: {error}"))?;
        let mut fields = marker_value.split_whitespace();
        let generation = fields
            .next()
            .ok_or_else(|| "crash marker omitted generation".to_owned())?
            .parse::<u64>()
            .map_err(|error| format!("parse crash acknowledgement generation: {error}"))?;
        let disposition = match fields.next() {
            Some("visible") => CrashDisposition::Visible,
            Some("gone") => CrashDisposition::Gone,
            other => return Err(format!("invalid crash marker disposition {other:?}")),
        };
        if fields.next().is_some() {
            return Err("crash marker has trailing fields".to_owned());
        }
        Ok(CrashRecovery {
            generation: generation.max(reopened_generation),
            disposition,
        })
    }

    fn generation(&mut self) -> Result<u64, String> {
        self.store()?
            .snapshot()
            .map(|snapshot| snapshot.generation())
            .map_err(|error| error.to_string())
    }

    fn reset_query_cancelled(&mut self) {
        self.query_cancelled = false;
    }

    fn query_cancelled(&self) -> bool {
        self.query_cancelled
    }
}

struct SelfTestEngine<E> {
    inner: E,
    bug: SelfTestBug,
    last_generation: u64,
    documents: BTreeMap<u32, DocMutation>,
    leaked: Option<Hit>,
}

impl<E> SelfTestEngine<E> {
    fn new(inner: E, bug: SelfTestBug) -> Self {
        Self {
            inner,
            bug,
            last_generation: 0,
            documents: BTreeMap::new(),
            leaked: None,
        }
    }
}

impl<E: Engine> Engine for SelfTestEngine<E> {
    fn open(&mut self) -> Result<(), String> {
        self.inner.open()?;
        self.last_generation = self.inner.generation()?;
        Ok(())
    }

    fn ingest(&mut self, documents: &[DocMutation]) -> Result<MutationAck, String> {
        for document in documents {
            self.documents.insert(document.doc_id, *document);
        }
        if self.bug == SelfTestBug::DropAcknowledgedWrite {
            self.last_generation = self.last_generation.saturating_add(1);
            return Ok(MutationAck {
                generation: self.last_generation,
                changed: true,
            });
        }
        let mut ack = self.inner.ingest(documents)?;
        if self.bug == SelfTestBug::MisreportGeneration {
            ack.generation = self.last_generation;
        } else {
            self.last_generation = ack.generation;
        }
        Ok(ack)
    }

    fn epoch_mismatch_probe(&mut self, document: DocMutation) -> Result<(), String> {
        self.inner.epoch_mismatch_probe(document)
    }

    fn prepare_epoch_b(&mut self, documents: &[DocMutation]) -> Result<MutationAck, String> {
        self.inner.prepare_epoch_b(documents)
    }

    fn switch_epoch(&mut self, epoch: ModelEpoch) -> Result<MutationAck, String> {
        self.inner.switch_epoch(epoch)
    }

    fn drop_epoch_a(&mut self) -> Result<MutationAck, String> {
        self.inner.drop_epoch_a()
    }

    fn rollback_dropped_a_probe(&mut self) -> Result<(), String> {
        self.inner.rollback_dropped_a_probe()
    }

    fn visible_epoch_ids(&mut self) -> Result<Vec<EpochId>, String> {
        self.inner.visible_epoch_ids()
    }

    fn delete(&mut self, doc_id: u32) -> Result<MutationAck, String> {
        let ack = self.inner.delete(doc_id)?;
        self.last_generation = ack.generation;
        if self.bug == SelfTestBug::LeakTombstone {
            let document = self
                .documents
                .get(&doc_id)
                .copied()
                .ok_or_else(|| "self-test leak target was never ingested".to_owned())?;
            self.leaked = Some(Hit {
                doc_id,
                revision: document.revision,
                score: 0.0,
            });
        }
        Ok(ack)
    }

    fn seal(&mut self) -> Result<MutationAck, String> {
        self.inner.seal()
    }

    fn drop_partition(&mut self, start: i64, end: i64) -> Result<MutationAck, String> {
        self.inner.drop_partition(start, end)
    }

    fn purge(&mut self, doc_id: u32) -> Result<MutationAck, String> {
        self.inner.purge(doc_id)
    }

    fn maintain(&mut self, bytes: u64) -> Result<MutationAck, String> {
        self.inner.maintain(bytes)
    }

    fn search(
        &mut self,
        query: &[f32],
        k: usize,
        kind: SearchKind,
        seed: u64,
    ) -> Result<SearchObservation, String> {
        let mut observed = self.inner.search(query, k, kind, seed)?;
        if self.bug == SelfTestBug::WrongDocument
            && let Some(first) = observed.hits.first_mut()
        {
            first.doc_id = 4_000_000_001;
        }
        if self.bug == SelfTestBug::LeakTombstone
            && let Some(leaked) = self.leaked
        {
            observed.hits.push(leaked);
        }
        Ok(observed)
    }

    fn filtered_search(
        &mut self,
        query: &[f32],
        k: usize,
        maximum_timestamp: i64,
    ) -> Result<SearchObservation, String> {
        self.inner.filtered_search(query, k, maximum_timestamp)
    }

    fn predicate_search(
        &mut self,
        query: &[f32],
        k: usize,
        predicate: program::PredicateKind,
    ) -> Result<SearchObservation, String> {
        self.inner.predicate_search(query, k, predicate)
    }

    fn lexical_search(&mut self, query_slot: u8, k: usize) -> Result<Vec<LexicalHit>, String> {
        self.inner.lexical_search(query_slot, k)
    }

    fn hybrid_search(&mut self, query_slot: u8, k: usize) -> Result<HybridObservation, String> {
        self.inner.hybrid_search(query_slot, k)
    }

    fn stats(&mut self) -> Result<StatsObservation, String> {
        self.inner.stats()
    }

    fn close(&mut self) -> Result<(), String> {
        self.inner.close()
    }

    fn reopen(&mut self) -> Result<(), String> {
        self.inner.reopen()
    }

    fn crash_at_boundary(
        &mut self,
        mutation: DocMutation,
        boundary: program::CrashBoundary,
        campaign: CampaignKind,
        seed: u64,
        op_index: usize,
        profile: FaultProfile,
    ) -> Result<CrashRecovery, String> {
        self.inner
            .crash_at_boundary(mutation, boundary, campaign, seed, op_index, profile)
    }

    fn generation(&mut self) -> Result<u64, String> {
        self.inner.generation()
    }

    fn reset_query_cancelled(&mut self) {
        self.inner.reset_query_cancelled();
    }

    fn query_cancelled(&self) -> bool {
        self.inner.query_cancelled()
    }
}

pub fn run_self_test(bug: SelfTestBug) -> Violation {
    let (seed, expected) = match bug {
        SelfTestBug::DropAcknowledgedWrite => (90_001, Invariant::I1),
        SelfTestBug::WrongDocument => (90_002, Invariant::I3),
        SelfTestBug::LeakTombstone => (90_003, Invariant::I2),
        SelfTestBug::MisreportGeneration => (90_004, Invariant::I9),
    };
    let directory = tempfile::tempdir().expect("self-test store directory");
    let real = RealEngine::without_faults(directory.path().to_path_buf());
    let mut engine = SelfTestEngine::new(real, bug);
    engine.open().expect("self-test open");
    let mut model = Model::default();
    let first = DocMutation {
        doc_id: 1,
        revision: 1,
        timestamp: 10,
    };
    let previous = engine.generation().expect("self-test generation");
    let ack = engine.ingest(&[first]).expect("self-test ingest");
    model.acknowledge(first.doc_id, first.revision, first.timestamp);
    if bug == SelfTestBug::MisreportGeneration {
        return generation_violation(seed, FaultProfile::None, 1, previous, ack)
            .expect("misreported generation must trip I9");
    }
    if bug == SelfTestBug::WrongDocument {
        let second = DocMutation {
            doc_id: 2,
            revision: 1,
            timestamp: 11,
        };
        engine.ingest(&[second]).expect("second self-test ingest");
        model.acknowledge(second.doc_id, second.revision, second.timestamp);
    }
    if bug == SelfTestBug::LeakTombstone {
        engine.delete(first.doc_id).expect("self-test delete");
        model.delete(first.doc_id);
    }
    let query = program::query(0);
    let observed = engine
        .search(
            &query,
            if bug == SelfTestBug::WrongDocument {
                1
            } else {
                model.len()
            },
            SearchKind::Scan,
            seed,
        )
        .expect("self-test search");
    let violations = check_search(
        seed,
        FaultProfile::None,
        3,
        &model,
        &query,
        if bug == SelfTestBug::WrongDocument {
            1
        } else {
            model.len()
        },
        SearchKind::Scan,
        &observed,
        false,
    );
    violations
        .into_iter()
        .find(|violation| violation.invariant == expected)
        .unwrap_or_else(|| panic!("planted {bug:?} did not trip {expected:?}"))
}

pub fn run_program(
    seed: u64,
    profile: FaultProfile,
    artifact_root: &Path,
) -> Result<RunOutcome, String> {
    run_program_for(CampaignKind::Overall, seed, profile, artifact_root)
}

pub fn run_program_for(
    campaign: CampaignKind,
    seed: u64,
    profile: FaultProfile,
    artifact_root: &Path,
) -> Result<RunOutcome, String> {
    run_program_for_with_clock(
        campaign,
        seed,
        profile,
        Some(profile),
        profile,
        artifact_root,
        Arc::new(ManualMonotonicClock::new()),
        None,
    )
}

pub fn run_program_for_seed(
    campaign: CampaignKind,
    seed: u64,
    artifact_root: &Path,
) -> Result<RunOutcome, String> {
    let profile = profile_for_seed(seed);
    run_program_for_with_clock(
        campaign,
        seed,
        profile,
        None,
        profile,
        artifact_root,
        Arc::new(ManualMonotonicClock::new()),
        None,
    )
}

pub fn run_program_for_with_feature_profile(
    campaign: CampaignKind,
    seed: u64,
    profile: FaultProfile,
    feature_profile: FaultProfile,
    artifact_root: &Path,
) -> Result<RunOutcome, String> {
    run_program_for_with_feature_profile_and_override(
        campaign,
        seed,
        profile,
        Some(profile),
        feature_profile,
        artifact_root,
    )
}

pub fn run_program_for_with_feature_profile_and_override(
    campaign: CampaignKind,
    seed: u64,
    profile: FaultProfile,
    profile_override: Option<FaultProfile>,
    feature_profile: FaultProfile,
    artifact_root: &Path,
) -> Result<RunOutcome, String> {
    run_program_for_with_clock(
        campaign,
        seed,
        profile,
        profile_override,
        feature_profile,
        artifact_root,
        Arc::new(ManualMonotonicClock::new()),
        None,
    )
}

pub fn run_program_with_schedule(
    seed: u64,
    profile: FaultProfile,
    artifact_root: &Path,
    schedule: FaultSchedule,
) -> Result<RunOutcome, String> {
    run_program_for_with_clock(
        CampaignKind::Overall,
        seed,
        profile,
        Some(profile),
        profile,
        artifact_root,
        Arc::new(ManualMonotonicClock::new()),
        Some(schedule),
    )
}

fn run_program_with_clock(
    seed: u64,
    profile: FaultProfile,
    artifact_root: &Path,
    clock: Arc<ManualMonotonicClock>,
) -> Result<RunOutcome, String> {
    run_program_for_with_clock(
        CampaignKind::Overall,
        seed,
        profile,
        Some(profile),
        profile,
        artifact_root,
        clock,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_program_for_with_clock(
    campaign: CampaignKind,
    seed: u64,
    profile: FaultProfile,
    profile_override: Option<FaultProfile>,
    feature_profile: FaultProfile,
    artifact_root: &Path,
    clock: Arc<ManualMonotonicClock>,
    schedule_override: Option<FaultSchedule>,
) -> Result<RunOutcome, String> {
    let _process_guard = if campaign == CampaignKind::VectorExecution {
        None
    } else {
        Some(
            super::feature_process_lock()
                .lock()
                .map_err(|_| "feature operation process lock was poisoned".to_owned())?,
        )
    };
    let program = Program::generate_for(campaign, seed);
    let graph_rows = usize::try_from(program::GRAPH_ROWS)
        .map_err(|_| "GRAPH_ROWS exceeds the runner's usize range".to_owned())?;
    let artifacts = RunArtifacts::create_for(artifact_root, campaign, seed, profile)?;
    let program_bytes = artifacts.write_program(&program)?;
    let reproduction = reproduction_for(campaign, seed, profile_override);
    artifacts.write_reproduction(&reproduction)?;
    let mut episode_bytes = if campaign == CampaignKind::Overall {
        artifacts.write_episode_metadata(
            campaign,
            seed,
            profile,
            profile_override.is_some(),
            &reproduction,
            None,
        )?
    } else {
        Vec::new()
    };
    let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
    let schedule = schedule_override.unwrap_or_else(|| {
        fault_vfs::plan_schedule(seed, environment_for_profile(profile, seed), &program)
    });
    let storage_episode = if campaign == CampaignKind::StorageDurability {
        Some(storage_adapter::build_storage_episode_fixtures(seed)?)
    } else {
        None
    };
    let hybrid_episode = if campaign == CampaignKind::HybridFusion {
        Some(hybrid_adapter::build_hybrid_episode(seed)?)
    } else {
        None
    };
    let mut fault_plan =
        FaultPlan::for_program(campaign, seed, feature_profile, &program, schedule.clone());
    artifacts.write_fault_plan(&fault_plan.schedule.events, &fault_plan.feature)?;
    let expected_feature_fault_receipts = fault_plan
        .feature
        .iter()
        .map(|event| event.fault.required_receipt_cardinality() as u64)
        .sum();
    let scheduled_vfs = Arc::new(fault_vfs::simulated_scheduled_with_clock(
        schedule,
        Arc::clone(&clock),
    ));
    let mut engine = RealEngine::new(
        directory.path().to_path_buf(),
        Arc::clone(&scheduled_vfs),
        Arc::clone(&clock),
    );
    let mut model = Model::default();
    let mut violations = Vec::new();
    let mut coverage = CoverageRegistry::default();
    let mut oracle_records = Vec::<OracleRecord>::new();
    let mut control_records = Vec::<String>::new();
    let mut receipt_records = Vec::<String>::new();
    let mut integrated_feature_fault_receipts = 0_u64;
    let mut same_seed_clean_controls = 0_u64;
    let mut refused_comparison_counts = BTreeMap::<String, u64>::new();
    let mut mutation_records = Vec::<String>::new();
    let mut family_artifact_records = BTreeMap::<&'static str, Vec<String>>::new();
    coverage.hit(format!("fault.profile.{}", profile.key()));
    let mut executed_operations = 0_usize;
    let mut last_generation = 0_u64;
    let mut content_fault_fired = false;
    let mut graph_searches = 0_usize;
    let mut filtered_searches = 0_usize;
    let mut filtered_graph_searches = 0_usize;
    let mut predicate_searches = 0_usize;
    let mut hybrid_searches = 0_usize;
    let mut text_documents_ingested = 0_usize;
    let mut store_lexical_searches = 0_usize;
    let mut store_hybrid_searches = 0_usize;
    let mut phrase_searches = 0_usize;
    let mut prefix_searches = 0_usize;
    let mut fuzzy_searches = 0_usize;
    let mut phonetic_encodes = 0_usize;
    let mut snippets_built = 0_usize;
    let mut hybrid_sealed_vector_documents = 0_usize;
    let mut hybrid_lexical_documents = 0_usize;
    let mut epoch_preparations = 0_usize;
    let mut epoch_alias_switches = 0_usize;
    let mut epoch_rollbacks = 0_usize;
    let mut epoch_drops = 0_usize;
    let mut rejected_dropped_epoch_rollbacks = 0_usize;
    let mut pending_busy_child = None::<BusyChildProcess>;
    let mut simulated_crash_recovered = false;
    let mut last_graph_publishing_maintain_op = None;
    let mut seal_precondition_lost_to_crash = false;
    let mut purge_refusal_after_fault = false;

    if campaign == CampaignKind::StorageDurability {
        if let Some(violation) = first_durable_ack_survives_wal_create_crash(seed, profile)? {
            violations.push(violation);
        } else {
            coverage.hit("crash.boundary.wal_create_post_ack");
        }
    }

    for (op_index, op) in program.ops.iter().enumerate() {
        executed_operations = executed_operations.saturating_add(1);
        coverage.hit(format!("attempt.op.{}", op.kind()));
        let cancel_scheduled = fault_plan
            .schedule
            .events
            .iter()
            .any(|event| event.op_index == op_index && event.layer == fault_vfs::Layer::Cancel);
        let mut cancel_stats_before = if cancel_scheduled
            && matches!(
                op,
                Op::Search { .. }
                    | Op::FilteredSearch { .. }
                    | Op::PredicateSearch { .. }
                    | Op::HybridSearch { .. }
                    | Op::DeadlineProbe { .. }
            ) {
            scheduled_vfs.set_operation(usize::MAX);
            match warm_cancel_query(&mut engine, &model, op, seed) {
                Ok(()) => {
                    engine.reset_query_cancelled();
                    engine.stats().ok()
                }
                Err(_) => None,
            }
        } else {
            None
        };
        scheduled_vfs.set_operation(op_index);
        let spawn_event = fault_plan.schedule.events.iter().any(|event| {
            event.op_index == op_index
                && event.layer == fault_vfs::Layer::Busy
                && event.mode == fault_vfs::FaultMode::SpawnInFlight
        });
        let arm_spawn_for_next_reopen = matches!(op, Op::Close)
            && fault_plan.schedule.events.iter().any(|event| {
                event.op_index == op_index.saturating_add(1)
                    && event.layer == fault_vfs::Layer::Busy
                    && event.mode == fault_vfs::FaultMode::SpawnInFlight
            });
        if arm_spawn_for_next_reopen && engine.store.is_some() {
            if pending_busy_child.is_some() {
                return Err("a second SpawnInFlight child was armed before Reopen".to_owned());
            }
            pending_busy_child = Some(spawn_busy_child(&engine)?);
        }
        let second_opener_event = fault_plan.schedule.events.iter().any(|event| {
            event.op_index == op_index
                && event.layer == fault_vfs::Layer::Busy
                && event.mode == fault_vfs::FaultMode::SecondOpenerInProcess
        });
        let busy_hook_result = if second_opener_event {
            let result = require_second_opener_refusal(&mut engine);
            scheduled_vfs
                .fire_runner_event(op_index, fault_vfs::FaultMode::SecondOpenerInProcess)?;
            coverage.hit(format!("fault.busy.second-opener.{}", op.kind()));
            result
        } else {
            Ok(())
        };
        engine.reset_query_cancelled();
        let selected_feature_faults = fault_plan
            .feature
            .iter()
            .filter(|fault| fault.op_index == op_index)
            .map(|event| event.fault)
            .collect::<Vec<_>>();
        if spawn_event && pending_busy_child.is_none() {
            scheduled_vfs.set_operation(usize::MAX);
        }
        let content_read_event = fault_plan.schedule.events.iter().find(|event| {
            event.op_index == op_index
                && event.layer == fault_vfs::Layer::Content
                && event.site == fault_vfs::FaultSite::Read
                && matches!(
                    op,
                    Op::Search { .. }
                        | Op::FilteredSearch { .. }
                        | Op::PredicateSearch { .. }
                        | Op::HybridSearch { .. }
                        | Op::DeadlineProbe { .. }
                )
        });
        let content_read_reopen = if spawn_event && pending_busy_child.is_none() {
            Ok(())
        } else if let Some(event) = content_read_event {
            let attempts = if event.nth_match == fault_vfs::LAST_MATCH {
                event.expected_matches.unwrap_or(1)
            } else {
                event.nth_match
            };
            let mut result = Ok(());
            for _ in 0..attempts.max(1) {
                match engine.reopen() {
                    Ok(()) => {
                        if scheduled_vfs
                            .events()
                            .iter()
                            .any(|observed| observed.id == event.id && observed.fired)
                        {
                            break;
                        }
                    }
                    Err(error) => {
                        scheduled_vfs.set_operation(usize::MAX);
                        let recovery = engine.reopen();
                        scheduled_vfs.set_operation(op_index);
                        result = match recovery {
                            Ok(()) => Err(error),
                            Err(recovery_error) => Err(format!(
                                "content-read refusal {error}; clean reopen failed: {recovery_error}"
                            )),
                        };
                        break;
                    }
                }
            }
            result
        } else {
            Ok(())
        };
        if content_read_reopen.is_ok() && cancel_stats_before.is_some() {
            scheduled_vfs.set_operation(usize::MAX);
            cancel_stats_before = match warm_cancel_query(&mut engine, &model, op, seed) {
                Ok(()) => {
                    engine.reset_query_cancelled();
                    engine.stats().ok()
                }
                Err(_) => None,
            };
            scheduled_vfs.set_operation(op_index);
        }
        let operation_attempted = content_read_reopen.is_ok();
        let operation_result = match content_read_reopen {
            Err(error) => Err(error),
            Ok(()) => match op {
                Op::Open => engine.open().map(|_| None),
                Op::Ingest {
                    first_id,
                    count,
                    revision,
                    timestamp,
                } => {
                    let documents = (*first_id..first_id.saturating_add(*count))
                        .map(|doc_id| DocMutation {
                            doc_id,
                            revision: *revision,
                            timestamp: *timestamp,
                        })
                        .collect::<Vec<_>>();
                    engine.ingest(&documents).map(|ack| {
                        text_documents_ingested =
                            text_documents_ingested.saturating_add(documents.len());
                        for document in &documents {
                            model.acknowledge(
                                document.doc_id,
                                document.revision,
                                document.timestamp,
                            );
                        }
                        Some(ack)
                    })
                }
                Op::EpochMismatchProbe {
                    doc_id,
                    revision,
                    timestamp,
                } => engine
                    .epoch_mismatch_probe(DocMutation {
                        doc_id: *doc_id,
                        revision: *revision,
                        timestamp: *timestamp,
                    })
                    .map(|()| None),
                Op::PrepareEpochB if seal_precondition_lost_to_crash => {
                    Err("epoch transition blocked by an uncommitted crashed Seal".to_owned())
                }
                Op::PrepareEpochB => {
                    let documents = model
                        .live_documents()
                        .into_iter()
                        .map(|(doc_id, revision, timestamp)| DocMutation {
                            doc_id,
                            revision,
                            timestamp,
                        })
                        .collect::<Vec<_>>();
                    engine.prepare_epoch_b(&documents).and_then(|ack| {
                        model.prepare_epoch_b();
                        epoch_preparations = epoch_preparations.saturating_add(1);
                        let visible = engine.visible_epoch_ids()?;
                        if let Some(violation) =
                            alias_visibility_violation(seed, profile, op_index, &model, &visible)
                        {
                            violations.push(violation);
                        }
                        Ok(Some(ack))
                    })
                }
                Op::SwitchAliasToB => engine.switch_epoch(ModelEpoch::B).and_then(|ack| {
                    if !model.switch_epoch(ModelEpoch::B) {
                        return Err("model has no prepared epoch B".to_owned());
                    }
                    epoch_alias_switches = epoch_alias_switches.saturating_add(1);
                    let visible = engine.visible_epoch_ids()?;
                    if let Some(violation) =
                        alias_visibility_violation(seed, profile, op_index, &model, &visible)
                    {
                        violations.push(violation);
                    }
                    Ok(Some(ack))
                }),
                Op::RollbackToA => engine.switch_epoch(ModelEpoch::A).and_then(|ack| {
                    if !model.switch_epoch(ModelEpoch::A) {
                        return Err("model epoch A was already dropped".to_owned());
                    }
                    epoch_rollbacks = epoch_rollbacks.saturating_add(1);
                    let visible = engine.visible_epoch_ids()?;
                    if let Some(violation) =
                        alias_visibility_violation(seed, profile, op_index, &model, &visible)
                    {
                        violations.push(violation);
                    }
                    Ok(Some(ack))
                }),
                Op::DropEpochA => engine.drop_epoch_a().and_then(|ack| {
                    if !model.drop_epoch_a() {
                        return Err("model refused to drop epoch A".to_owned());
                    }
                    epoch_drops = epoch_drops.saturating_add(1);
                    let visible = engine.visible_epoch_ids()?;
                    if let Some(violation) =
                        alias_visibility_violation(seed, profile, op_index, &model, &visible)
                    {
                        violations.push(violation);
                    }
                    Ok(Some(ack))
                }),
                Op::RollbackDroppedAProbe => engine.rollback_dropped_a_probe().and_then(|()| {
                    rejected_dropped_epoch_rollbacks =
                        rejected_dropped_epoch_rollbacks.saturating_add(1);
                    let visible = engine.visible_epoch_ids()?;
                    if let Some(violation) =
                        alias_visibility_violation(seed, profile, op_index, &model, &visible)
                    {
                        violations.push(violation);
                    }
                    Ok(None)
                }),
                Op::Upsert {
                    doc_id,
                    revision,
                    timestamp,
                }
                | Op::Revise {
                    doc_id,
                    revision,
                    timestamp,
                } => {
                    let document = DocMutation {
                        doc_id: *doc_id,
                        revision: *revision,
                        timestamp: *timestamp,
                    };
                    engine.ingest(&[document]).map(|ack| {
                        text_documents_ingested = text_documents_ingested.saturating_add(1);
                        model.acknowledge(*doc_id, *revision, *timestamp);
                        Some(ack)
                    })
                }
                Op::Delete { doc_id } => engine.delete(*doc_id).map(|ack| {
                    model.delete(*doc_id);
                    Some(ack)
                }),
                Op::Seal => engine.seal().map(|ack| {
                    model.seal();
                    seal_precondition_lost_to_crash = false;
                    Some(ack)
                }),
                Op::Maintain { bytes } => engine.maintain(*bytes).map(Some),
                Op::Search { query, k, kind } => {
                    let query = program::query(*query);
                    let requested = if *k == usize::MAX { model.len() } else { *k };
                    engine
                        .search(&query, requested, *kind, seed)
                        .map(|observed| {
                            if observed.graph_segments > 0 {
                                graph_searches = graph_searches.saturating_add(1);
                                coverage.hit("search.graph_traversal");
                            }
                            violations.extend(check_search(
                                seed,
                                profile,
                                op_index,
                                &model,
                                &query,
                                requested,
                                *kind,
                                &observed,
                                content_fault_fired,
                            ));
                            None
                        })
                }
                Op::FilteredSearch {
                    query,
                    k,
                    maximum_timestamp,
                } => {
                    let query = program::query(*query);
                    let requested = if *k == usize::MAX { model.len() } else { *k };
                    engine
                        .filtered_search(&query, requested, *maximum_timestamp)
                        .map(|observed| {
                            filtered_searches = filtered_searches.saturating_add(1);
                            if observed.graph_segments > 0 {
                                filtered_graph_searches = filtered_graph_searches.saturating_add(1);
                                coverage.hit("search.filtered_graph");
                            }
                            violations.extend(check_filtered_search(
                                seed,
                                profile,
                                op_index,
                                &model,
                                &query,
                                requested,
                                *maximum_timestamp,
                                &observed,
                            ));
                            None
                        })
                }
                Op::HybridSearch { query, k } => {
                    let requested = if *k == usize::MAX { model.len() } else { *k };
                    run_hybrid_search(&mut engine, &model, *query, requested, seed).map(|check| {
                        if let Some(check) = check {
                            hybrid_searches = hybrid_searches.saturating_add(1);
                            store_lexical_searches =
                                store_lexical_searches.saturating_add(check.store_lexical_searches);
                            store_hybrid_searches =
                                store_hybrid_searches.saturating_add(check.store_hybrid_searches);
                            if check.store_lexical_searches > 0 {
                                coverage.hit("store.lexical_search");
                            }
                            if check.store_hybrid_searches > 0 {
                                coverage.hit("store.hybrid_search");
                            }
                            hybrid_sealed_vector_documents = hybrid_sealed_vector_documents
                                .saturating_add(check.sealed_vector_documents);
                            hybrid_lexical_documents =
                                hybrid_lexical_documents.saturating_add(check.lexical_documents);
                            if let Some(detail) = check.mismatch {
                                violations.push(violation(
                                    Invariant::I3,
                                    seed,
                                    profile,
                                    op_index,
                                    detail,
                                ));
                            }
                        }
                        None
                    })
                }
                Op::DeadlineProbe { query } => {
                    if cancel_scheduled {
                        if matches!(profile, FaultProfile::Clock | FaultProfile::Full) {
                            engine.arm_deadline_probe()?;
                            coverage.hit("fault.deadline.skipped-for-cancel");
                        }
                        let query = program::query(*query);
                        engine
                            .search(&query, 1, SearchKind::Scan, seed)
                            .map(|observed| {
                                violations.extend(check_search(
                                    seed,
                                    profile,
                                    op_index,
                                    &model,
                                    &query,
                                    1,
                                    SearchKind::Scan,
                                    &observed,
                                    content_fault_fired,
                                ));
                                None
                            })
                    } else if matches!(profile, FaultProfile::Clock | FaultProfile::Full)
                        && model.is_empty()
                        && simulated_crash_recovered
                    {
                        // A preceding crash may legally recover the empty durable
                        // prefix. The product's empty query has no scan checkpoint,
                        // so this episode must not fabricate deadline coverage.
                        Ok(None)
                    } else if matches!(profile, FaultProfile::Clock | FaultProfile::Full) {
                        engine.arm_deadline_probe()?;
                        match engine.search(&program::query(*query), 1, SearchKind::Scan, seed) {
                            Err(error) if error.contains("deadline expired") => Ok(None),
                            Err(error) => Err(format!(
                                "deadline probe returned the wrong typed failure: {error}"
                            )),
                            Ok(_) => Err("already-expired deadline was admitted".to_owned()),
                        }
                    } else {
                        Ok(None)
                    }
                }
                Op::PredicateSearch {
                    query,
                    k,
                    predicate,
                } => {
                    let query_vector = program::query(*query);
                    let requested = if *k == usize::MAX { model.len() } else { *k };
                    engine
                        .predicate_search(&query_vector, requested, *predicate)
                        .map(|observed| {
                            predicate_searches = predicate_searches.saturating_add(1);
                            let expected =
                                model.expected_predicate(&query_vector, requested, *predicate);
                            if let Some(detail) = exact_mismatch(&expected, &observed.hits) {
                                violations.push(violation(
                                    Invariant::I5,
                                    seed,
                                    profile,
                                    op_index,
                                    format!("{} predicate mismatch: {detail}", predicate.key()),
                                ));
                            }
                            if let Some(violation) = diagnostics_violation(
                                seed,
                                profile,
                                op_index,
                                requested,
                                model
                                    .expected_predicate(&query_vector, model.len(), *predicate)
                                    .len(),
                                &observed,
                            ) {
                                violations.push(violation);
                            }
                            None
                        })
                }
                Op::FtsExtrasProbe { slot } => run_fts_extras_probe(*slot).map(|()| {
                    phrase_searches = phrase_searches.saturating_add(1);
                    prefix_searches = prefix_searches.saturating_add(1);
                    fuzzy_searches = fuzzy_searches.saturating_add(1);
                    phonetic_encodes = phonetic_encodes.saturating_add(1);
                    snippets_built = snippets_built.saturating_add(1);
                    coverage.hit("fts.phrase");
                    coverage.hit("fts.prefix");
                    coverage.hit("fts.fuzzy");
                    coverage.hit("fts.phonetic");
                    coverage.hit("fts.snippet");
                    None
                }),
                Op::Feature(operation) => {
                    let record_start = oracle_records.len();
                    let control_start = control_records.len();
                    let result = run_campaign_operation_with_clean_control(
                        *operation,
                        CampaignOperationContext {
                            selected_faults: &selected_feature_faults,
                            generic_fault: fault_plan
                                .schedule
                                .events
                                .iter()
                                .find(|event| event.op_index == op_index),
                            storage_episode: storage_episode.as_ref(),
                            hybrid_episode: hybrid_episode.as_ref(),
                            seed,
                            profile,
                            op_index,
                            oracle_records: &mut oracle_records,
                            control_records: &mut control_records,
                            mutation_records: &mut mutation_records,
                            family_artifact_records: &mut family_artifact_records,
                            coverage: &mut coverage,
                            control_store: None,
                        },
                    );
                    for record in oracle_records.get(record_start..).unwrap_or_default() {
                        if !record.passed {
                            violations.push(Violation {
                                invariant: Invariant::Feature(super::campaign::InvariantId::new(
                                    record.invariant,
                                )),
                                seed,
                                profile,
                                op_index,
                                detail: format!("{}: {}", record.checker_id, record.detail),
                            });
                        }
                    }
                    result.and_then(|operation_outcome| {
                    if let Some(event) = operation_outcome.generic_fault_event.as_ref() {
                        scheduled_vfs.adopt_cancel_event(event)?;
                    }
                    for (invariant, count) in &operation_outcome.refused_comparison_counts {
                        let total = refused_comparison_counts.entry(invariant.clone()).or_default();
                        *total = total.saturating_add(*count);
                    }
                    if !selected_feature_faults.is_empty() {
                        let observed_controls = control_records.len().saturating_sub(control_start);
                        let qualifying_controls = operation_outcome.qualifying_controls;
                        if qualifying_controls != selected_feature_faults.len() {
                            return Err(format!(
                                "feature operation {} produced {qualifying_controls} qualifying same-seed controls ({observed_controls} total) for {} selected faults: {:?}",
                                operation.key(),
                                selected_feature_faults.len(),
                                control_records.get(control_start..).unwrap_or_default(),
                            ));
                        }
                        same_seed_clean_controls = same_seed_clean_controls.saturating_add(
                            u64::try_from(qualifying_controls)
                                .map_err(|_| "same-seed control count exceeds u64".to_owned())?,
                        );
                    }
                    for receipt in operation_outcome.receipts {
                    if receipt.campaign() != campaign.key()
                        || receipt.operation() != operation.key()
                        || receipt.site().is_empty()
                    {
                        return Err(format!(
                            "feature receipt origin mismatch: expected {}/{}, observed {}/{} at {:?}",
                            campaign.key(),
                            operation.key(),
                            receipt.campaign(),
                            receipt.operation(),
                            receipt.site(),
                        ));
                    }
                    if let Some(receipt_fault) = receipt.fault() {
                        let event = fault_plan
                            .feature
                            .iter_mut()
                            .find(|event| {
                                event.fault.key() == receipt_fault && event.op_index == op_index
                            })
                            .ok_or_else(|| {
                                format!(
                                    "unplanned feature-fault receipt {receipt_fault} at op {op_index}"
                                )
                            })?;
                        let cardinality = usize::try_from(receipt.cardinality().ok_or_else(|| {
                            "feature receipt omitted its cardinality".to_owned()
                        })?)
                        .map_err(|_| {
                            "feature receipt cardinality does not fit usize".to_owned()
                        })?;
                        let expected = event.fault.required_receipt_cardinality();
                        event.fire_count = event.fire_count.saturating_add(cardinality);
                        integrated_feature_fault_receipts = integrated_feature_fault_receipts
                            .saturating_add(u64::try_from(cardinality).map_err(|_| {
                                "feature receipt cardinality exceeds u64".to_owned()
                            })?);
                        event.fired = event.fire_count == expected;
                        if event.fire_count > expected {
                            return Err(format!(
                                "feature fault {} fired {} times, expected exactly {expected}",
                                event.fault.key(),
                                event.fire_count
                            ));
                        }
                    }
                    receipt_records.push(production_receipt_json(&receipt));
                    }
                    for event in fault_plan
                        .feature
                        .iter_mut()
                        .filter(|event| event.op_index == op_index)
                    {
                        let expected = event.fault.required_receipt_cardinality();
                        event.fired = event.fire_count == expected;
                        if !event.fired {
                            return Err(format!(
                                "feature fault {} fired {} times, expected exactly {expected}",
                                event.fault.key(),
                                event.fire_count
                            ));
                        }
                        coverage.hit(event.fault.coverage_key());
                    }
                    Ok(None)
                })
                }
                Op::Stats => engine.stats().map(|stats| {
                    if let Some(violation) = stats_violation(seed, profile, op_index, stats) {
                        violations.push(violation);
                    }
                    None
                }),
                Op::Close => engine.close().map(|_| None),
                Op::Reopen if spawn_event && pending_busy_child.is_some() => (|| {
                    let child = pending_busy_child
                        .take()
                        .ok_or_else(|| "SpawnInFlight reached Reopen without a child".to_owned())?;
                    scheduled_vfs
                        .fire_runner_event(op_index, fault_vfs::FaultMode::SpawnInFlight)?;
                    let first = reopen_after_spawn(&mut engine);
                    child.finish()?;
                    first?;
                    coverage.hit("fault.busy.spawn.first-attempt");
                    match engine.stats() {
                        Ok(stats) => {
                            if let Some(violation) =
                                lifecycle_stats_violation(seed, profile, op_index, stats)
                            {
                                violations.push(violation);
                            }
                        }
                        Err(error) => violations.push(violation(
                            Invariant::I8,
                            seed,
                            profile,
                            op_index,
                            format!("reopen stats failed: {error}"),
                        )),
                    }
                    Ok(None)
                })(),
                Op::Reopen => engine.reopen().map(|_| {
                    match engine.stats() {
                        Ok(stats) => {
                            if let Some(violation) =
                                lifecycle_stats_violation(seed, profile, op_index, stats)
                            {
                                violations.push(violation);
                            }
                        }
                        Err(error) => violations.push(violation(
                            Invariant::I8,
                            seed,
                            profile,
                            op_index,
                            format!("reopen stats failed: {error}"),
                        )),
                    }
                    None
                }),
                Op::Crash {
                    doc_id,
                    revision,
                    timestamp,
                    boundary,
                } => {
                    let document = DocMutation {
                        doc_id: *doc_id,
                        revision: *revision,
                        timestamp: *timestamp,
                    };
                    engine
                        .crash_at_boundary(document, *boundary, campaign, seed, op_index, profile)
                        .map(|recovery| {
                            match recovery.disposition {
                                CrashDisposition::Unacknowledged => {}
                                CrashDisposition::Visible => {
                                    text_documents_ingested =
                                        text_documents_ingested.saturating_add(1);
                                    model.acknowledge(*doc_id, *revision, *timestamp);
                                }
                                CrashDisposition::Gone => {
                                    text_documents_ingested =
                                        text_documents_ingested.saturating_add(1);
                                    model.purge(*doc_id);
                                    if let Some(violation) = purge_proof_violation(
                                        seed,
                                        profile,
                                        op_index,
                                        directory.path(),
                                        *doc_id,
                                    ) {
                                        violations.push(violation);
                                    }
                                }
                            }
                            match full_scan(&mut engine, &model, seed) {
                                Ok(observed) => {
                                    if let Some(violation) = epoch_identity_violation(
                                        seed,
                                        profile,
                                        op_index,
                                        identity_for(model.published_epoch()),
                                        &observed,
                                    ) {
                                        violations.push(violation);
                                    }
                                    if let Some(violation) = durability_prefix_violation(
                                        seed, profile, op_index, &model, &observed,
                                    ) {
                                        violations.push(violation);
                                    }
                                }
                                Err(error) => violations.push(violation(
                                    Invariant::I4,
                                    seed,
                                    profile,
                                    op_index,
                                    format!(
                                        "crash recovery did not open as a clean prefix: {error}"
                                    ),
                                )),
                            }
                            Some(MutationAck {
                                generation: recovery.generation,
                                changed: recovery.disposition != CrashDisposition::Unacknowledged,
                            })
                        })
                }
                Op::DropPartition { start, end } => {
                    engine.drop_partition(*start, *end).map(|ack| {
                        if ack.changed {
                            model.drop_partition(*start, *end);
                        }
                        Some(ack)
                    })
                }
                Op::Purge { doc_id } => engine.purge(*doc_id).map(|ack| {
                    if ack.changed {
                        model.purge(*doc_id);
                    }
                    if let Some(violation) =
                        purge_proof_violation(seed, profile, op_index, directory.path(), *doc_id)
                    {
                        violations.push(violation);
                    }
                    Some(ack)
                }),
            },
        };

        if operation_attempted
            && (cancel_stats_before.is_some() || operation_result.is_ok())
            && let Some(cancel_outcome) = scheduled_vfs.finish_cancel()?
        {
            coverage.hit(cancel_outcome.coverage_key());
        }
        if engine.query_cancelled() && !cancel_scheduled {
            violations.push(violation(
                Invariant::I55,
                seed,
                profile,
                op_index,
                "query returned typed cancellation without a planned Cancel event".to_owned(),
            ));
        }
        if operation_attempted && let Some(before) = cancel_stats_before {
            let after = engine.stats()?;
            if before == after {
                coverage.hit("fault.cancel.generic.state-unchanged");
            } else {
                violations.push(violation(
                    Invariant::I55,
                    seed,
                    profile,
                    op_index,
                    format!(
                        "Cancel query changed Store::stats(): before={before:?} after={after:?}"
                    ),
                ));
            }
        }

        let fired_at_operation = scheduled_vfs
            .events()
            .into_iter()
            .filter(|event| event.op_index == op_index && event.fired)
            .collect::<Vec<_>>();
        let content_at_operation = fired_at_operation
            .iter()
            .any(|event| event.layer == fault_vfs::Layer::Content);
        content_fault_fired |= content_at_operation;
        let clock_timeout_expected = clock_jump_requires_timeout(&fired_at_operation);

        if let Err(error) = busy_hook_result
            && !persisted_content_fault_before(&scheduled_vfs.events(), op_index)
            && !purge_refusal_after_fault
        {
            if !runner_records_error(
                &scheduled_vfs.events(),
                op_index,
                RunnerErrorSource::BusyHook,
            ) {
                return Err("Busy-hook error suppression contract returned false".to_owned());
            }
            violations.push(violation(
                Invariant::I8,
                seed,
                profile,
                op_index,
                format!("busy hook returned unexpected error: {error}"),
            ));
        }

        let mut operation_succeeded = false;
        match operation_result {
            Ok(Some(ack)) => {
                if clock_timeout_expected {
                    violations.push(violation(
                        Invariant::I54,
                        seed,
                        profile,
                        op_index,
                        "clock jumped past its deadline but the query returned a result".to_owned(),
                    ));
                } else {
                    operation_succeeded = true;
                    if ack.changed {
                        if matches!(op, Op::Maintain { .. }) {
                            last_graph_publishing_maintain_op = Some(op_index);
                        }
                        if let Some(generation) =
                            generation_violation(seed, profile, op_index, last_generation, ack)
                        {
                            violations.push(generation);
                        }
                        last_generation = last_generation.max(ack.generation);
                    }
                }
            }
            Ok(None) => {
                if clock_timeout_expected {
                    violations.push(violation(
                        Invariant::I54,
                        seed,
                        profile,
                        op_index,
                        "clock jumped past its deadline but the query returned a result".to_owned(),
                    ));
                } else {
                    operation_succeeded = true;
                }
            }
            Err(error) => {
                if clock_timeout_expected && error.contains("deadline expired") {
                    operation_succeeded = true;
                } else if clock_timeout_expected {
                    violations.push(violation(
                        Invariant::I54,
                        seed,
                        profile,
                        op_index,
                        format!("clock jump returned the wrong typed failure: {error}"),
                    ));
                } else if fired_at_operation
                    .iter()
                    .any(|event| event.layer == fault_vfs::Layer::Crash)
                {
                    let crash_event = fired_at_operation
                        .iter()
                        .find(|event| event.layer == fault_vfs::Layer::Crash)
                        .ok_or_else(|| {
                            "fired Crash event disappeared before recovery".to_owned()
                        })?;
                    let wal_dirent_durable = engine.wal_dirent_is_durable()?;
                    match engine.recover_from_simulated_crash() {
                        Ok(()) => {
                            reconcile_simulated_crash(
                                &mut model,
                                op,
                                crash_event,
                                wal_dirent_durable,
                            )?;
                            reconcile_recovered_locations(&mut engine, &mut model)?;
                            simulated_crash_recovered = true;
                            if matches!(op, Op::Seal) {
                                seal_precondition_lost_to_crash = true;
                            }
                            let recovered_generation = engine.generation()?;
                            if recovered_generation < last_generation {
                                violations.push(violation(
                                    Invariant::I4,
                                    seed,
                                    profile,
                                    op_index,
                                    format!(
                                        "simulated crash recovery regressed durable generation from {last_generation} to {recovered_generation}"
                                    ),
                                ));
                            }
                            last_generation = last_generation.max(recovered_generation);
                            let observed = full_scan(&mut engine, &model, seed)?;
                            if let Some(violation) = durability_prefix_violation(
                                seed, profile, op_index, &model, &observed,
                            ) {
                                violations.push(violation);
                            }
                        }
                        Err(recovery_error) => {
                            if wal_rewrite_refusal(&recovery_error) {
                                coverage.hit("purge.recovery_refused_after_crash");
                                purge_refusal_after_fault = true;
                                if campaign == CampaignKind::Overall {
                                    break;
                                }
                                continue;
                            }
                            if persisted_content_fault_preceded(&scheduled_vfs.events(), op_index) {
                                coverage.hit("crash.recovery_refused_after_content_write");
                                if campaign == CampaignKind::Overall {
                                    break;
                                }
                                continue;
                            }
                            violations.push(violation(
                                Invariant::I4,
                                seed,
                                profile,
                                op_index,
                                format!(
                                    "simulated crash recovery refused after {}: {recovery_error}",
                                    op.kind()
                                ),
                            ));
                            break;
                        }
                    }
                } else if matches!(op, Op::PrepareEpochB)
                    && seal_precondition_lost_to_crash
                    && error == "epoch transition blocked by an uncommitted crashed Seal"
                {
                    break;
                } else if simulated_crash_recovered
                    && matches!(
                        op,
                        Op::Search {
                            kind: SearchKind::Graph,
                            ..
                        }
                    )
                    && (model.sealed_document_count() < graph_rows
                        || graph_build_crash_preceded(
                            &scheduled_vfs.events(),
                            op_index,
                            last_graph_publishing_maintain_op,
                        ))
                    && error.contains("has no graph region")
                {
                    if model.sealed_document_count() >= graph_rows {
                        coverage.hit("crash.graph_build_interrupted_refusal");
                    }
                    operation_succeeded = true;
                } else if simulated_crash_recovered && wal_rewrite_refusal(&error) {
                    if matches!(op, Op::Purge { .. }) {
                        coverage.hit("purge.refused_after_crash");
                    } else {
                        coverage.hit("purge.recovery_refused_after_crash");
                    }
                    purge_refusal_after_fault = true;
                    if campaign == CampaignKind::Overall {
                        break;
                    }
                    continue;
                } else if persisted_content_fault_preceded(&scheduled_vfs.events(), op_index)
                    || purge_refusal_after_fault
                    || !runner_records_operation_error(&scheduled_vfs.events(), op_index)
                {
                    if matches!(op, Op::Purge { .. })
                        && error
                            .starts_with("purge WAL rewrite would drop acknowledged WAL sequence ")
                    {
                        coverage.hit("purge.refused_after_content_write");
                    }
                    // A persisted content mutation may be impossible to clear.
                    // The typed refusal itself satisfies I7. Feature campaigns
                    // keep executing so an independently materialized family
                    // operation scheduled after the refusal still runs. The
                    // legacy overall campaign preserves its original stop
                    // semantics and never drives later operations through the
                    // poisoned generic Store.
                    let imminent_crash = fault_plan.schedule.events.iter().any(|event| {
                        event.layer == fault_vfs::Layer::Crash
                            && event.op_index > op_index
                            && event.op_index <= op_index.saturating_add(3)
                    });
                    if campaign == CampaignKind::Overall && !imminent_crash {
                        break;
                    }
                    continue;
                } else if runner_retries_faulted_operation(&fired_at_operation, op_index) {
                    match recover_and_retry_faulted_operation(&mut engine, &mut model, op) {
                        Ok(Some(ack)) => {
                            operation_succeeded = true;
                            last_generation = last_generation.max(ack.generation);
                        }
                        Ok(None) => operation_succeeded = true,
                        Err(recovery_error) => {
                            let crash_fired_during_retry =
                                scheduled_vfs.events().iter().any(|event| {
                                    event.op_index == op_index
                                        && event.fired
                                        && event.layer == fault_vfs::Layer::Crash
                                });
                            if crash_fired_during_retry {
                                let crash_event = scheduled_vfs
                                    .events()
                                    .into_iter()
                                    .find(|event| {
                                        event.op_index == op_index
                                            && event.fired
                                            && event.layer == fault_vfs::Layer::Crash
                                    })
                                    .ok_or_else(|| {
                                        "retry Crash event disappeared before recovery".to_owned()
                                    })?;
                                let wal_dirent_durable = engine.wal_dirent_is_durable()?;
                                match engine.recover_from_simulated_crash() {
                                    Ok(()) => {
                                        reconcile_simulated_crash(
                                            &mut model,
                                            op,
                                            &crash_event,
                                            wal_dirent_durable,
                                        )?;
                                        reconcile_recovered_locations(&mut engine, &mut model)?;
                                        simulated_crash_recovered = true;
                                        if matches!(op, Op::Seal) {
                                            seal_precondition_lost_to_crash = true;
                                        }
                                        let recovered_generation = engine.generation()?;
                                        if recovered_generation < last_generation {
                                            violations.push(violation(
                                                Invariant::I4,
                                                seed,
                                                profile,
                                                op_index,
                                                format!(
                                                    "simulated crash recovery regressed durable generation from {last_generation} to {recovered_generation}"
                                                ),
                                            ));
                                        }
                                        last_generation = last_generation.max(recovered_generation);
                                        let observed = full_scan(&mut engine, &model, seed)?;
                                        if let Some(violation) = durability_prefix_violation(
                                            seed, profile, op_index, &model, &observed,
                                        ) {
                                            violations.push(violation);
                                        }
                                    }
                                    Err(crash_recovery_error) => {
                                        if wal_rewrite_refusal(&crash_recovery_error) {
                                            coverage.hit("purge.recovery_refused_after_crash");
                                            purge_refusal_after_fault = true;
                                            if campaign == CampaignKind::Overall {
                                                break;
                                            }
                                            continue;
                                        }
                                        if persisted_content_fault_preceded(
                                            &scheduled_vfs.events(),
                                            op_index,
                                        ) {
                                            coverage
                                                .hit("crash.recovery_refused_after_content_write");
                                            if campaign == CampaignKind::Overall {
                                                break;
                                            }
                                            continue;
                                        }
                                        violations.push(violation(
                                            Invariant::I4,
                                            seed,
                                            profile,
                                            op_index,
                                            format!(
                                                "simulated crash recovery refused during {} retry: {crash_recovery_error}",
                                                op.kind()
                                            ),
                                        ));
                                        break;
                                    }
                                }
                                continue;
                            }
                            let content_fired_during_retry =
                                scheduled_vfs.events().iter().any(|event| {
                                    event.op_index == op_index
                                        && event.fired
                                        && event.layer == fault_vfs::Layer::Content
                                });
                            if content_fired_during_retry {
                                content_fault_fired = true;
                                if campaign == CampaignKind::Overall {
                                    break;
                                }
                                if !persisted_content_fault_preceded(
                                    &scheduled_vfs.events(),
                                    op_index,
                                ) {
                                    engine.reopen()?;
                                }
                                continue;
                            }
                            violations.push(violation(
                                Invariant::I8,
                                seed,
                                profile,
                                op_index,
                                format!(
                                    "{} fault {error}; automatic reopen/retry failed: {recovery_error}",
                                    op.kind()
                                ),
                            ));
                        }
                    }
                } else {
                    let invariant = match op {
                        Op::Open | Op::Close | Op::Reopen => Invariant::I8,
                        Op::EpochMismatchProbe { .. } => Invariant::I12,
                        Op::PrepareEpochB
                        | Op::SwitchAliasToB
                        | Op::RollbackToA
                        | Op::DropEpochA
                        | Op::RollbackDroppedAProbe => Invariant::I14,
                        Op::Crash { .. } => Invariant::I4,
                        Op::Stats => Invariant::I6,
                        Op::HybridSearch { .. } => Invariant::I3,
                        Op::DeadlineProbe { .. } => Invariant::I54,
                        Op::FtsExtrasProbe { .. } => Invariant::I3,
                        _ if content_at_operation => Invariant::I7,
                        _ => Invariant::I1,
                    };
                    violations.push(violation(
                        invariant,
                        seed,
                        profile,
                        op_index,
                        format!("{} returned unexpected error: {error}", op.kind()),
                    ));
                }
            }
        }
        if operation_succeeded {
            record_successful_operation_coverage(&mut coverage, op);
            if fired_at_operation
                .iter()
                .any(|event| event.layer == fault_vfs::Layer::Crash)
                && !matches!(op, Op::Crash { .. })
            {
                // The Crash fault fired at a site whose failure the engine
                // legitimately swallows after publication (for example the
                // drop_partition post-commit unlink), so the operation was
                // acknowledged while the simulated machine went down. The
                // acked state is durable; restart the crashed VFS and reopen
                // instead of letting every later operation fail against the
                // latched crash and be misreported as an I1 violation.
                engine.recover_from_simulated_crash()?;
                reconcile_recovered_locations(&mut engine, &mut model)?;
                simulated_crash_recovered = true;
                let recovered_generation = engine.generation()?;
                if recovered_generation < last_generation {
                    violations.push(violation(
                        Invariant::I4,
                        seed,
                        profile,
                        op_index,
                        format!(
                            "simulated crash recovery regressed durable generation from {last_generation} to {recovered_generation}"
                        ),
                    ));
                }
                last_generation = last_generation.max(recovered_generation);
                let observed = full_scan(&mut engine, &model, seed)?;
                if let Some(violation) =
                    durability_prefix_violation(seed, profile, op_index, &model, &observed)
                {
                    violations.push(violation);
                }
            }
        }
    }

    if let Some(child) = pending_busy_child {
        child.finish()?;
        return Err("SpawnInFlight child did not reach its scheduled Reopen".to_owned());
    }

    let faults = scheduled_vfs.events();
    let scheduled_faults_fired = faults.iter().filter(|event| event.fired).count();
    for event in &faults {
        if event.fired {
            coverage.hit(format!("fault.site.{}", event.site.key()));
            coverage.hit(format!("fault.mode.{}", event.mode.key()));
            coverage.hit(format!("fault.layer.{}", event.layer.key()));
            if event.layer == fault_vfs::Layer::Crash {
                coverage.hit(format!("crash.simulated.{}", event.site.key()));
            }
            if event.is_wal_create_before_directory_sync() {
                coverage.hit("crash.boundary.wal_create_after_file_sync_before_directory_sync");
            }
        }
    }
    for (mode, count) in crash_after_coverage_counts(&faults) {
        for _ in 0..count {
            coverage.hit(format!("crash.after.{mode}"));
        }
    }
    let fired_layers = faults
        .iter()
        .filter(|event| event.fired)
        .map(|event| event.layer)
        .collect::<BTreeSet<_>>();
    if !fired_layers.is_empty() {
        for count in 1..=fired_layers.len().min(4) {
            coverage.hit(format!("fault.layer.count.{count}"));
        }
    }
    if fault_plan.feature.iter().any(|event| event.fired) {
        for layer in &fired_layers {
            coverage.hit(format!("fault.layered.{}+feature", layer.key()));
        }
    }
    let faults_fired = faults
        .iter()
        .filter(|fault| fault.fired)
        .count()
        .saturating_add(
            fault_plan
                .feature
                .iter()
                .filter(|fault| fault.fired)
                .count(),
        );
    let faults_bytes = artifacts.write_fault_plan(&faults, &fault_plan.feature)?;
    let violations_bytes = artifacts.write_violations_for(campaign, &violations)?;
    let oracle_bytes = artifacts.write_oracle(&oracle_records)?;
    let controls_bytes = if campaign == CampaignKind::Overall {
        Vec::new()
    } else {
        artifacts.write_controls(&control_records)?
    };
    let receipts_bytes = if campaign == CampaignKind::Overall {
        Vec::new()
    } else {
        artifacts.write_receipts(&receipt_records)?
    };
    let mutations_bytes = if campaign == CampaignKind::Overall {
        Vec::new()
    } else {
        artifacts.write_mutations(&mutation_records)?
    };
    let mut family_artifact_bytes = BTreeMap::new();
    for (name, records) in family_artifact_records {
        let bytes = render_family_artifact(name, &records)?;
        let written = artifacts.write_family_artifact(campaign, name, &bytes)?;
        family_artifact_bytes.insert(name.to_owned(), written);
    }
    let mut comparison_counts = BTreeMap::<String, u64>::new();
    let mut comparison_pass_counts = BTreeMap::<String, u64>::new();
    for record in &oracle_records {
        let invariant = format!("I{}", record.invariant);
        let count = comparison_counts.entry(invariant.clone()).or_default();
        *count = count.saturating_add(1);
        if record.passed {
            let passes = comparison_pass_counts.entry(invariant).or_default();
            *passes = passes.saturating_add(1);
        }
    }
    for (invariant, refused) in &refused_comparison_counts {
        let count = comparison_counts.entry(invariant.clone()).or_default();
        *count = count.saturating_add(*refused);
    }
    let comparison_outcome_counts = comparison_counts
        .keys()
        .map(|invariant| {
            (
                invariant.clone(),
                ComparisonOutcomeCounts {
                    equal: comparison_pass_counts.get(invariant).copied().unwrap_or(0),
                    refused: refused_comparison_counts
                        .get(invariant)
                        .copied()
                        .unwrap_or(0),
                },
            )
        })
        .collect();
    let mut outcome = RunOutcome {
        campaign,
        seed,
        profile,
        profile_overridden: profile_override.is_some(),
        operations: executed_operations,
        faults_fired,
        scheduled_faults_fired,
        feature_faults_scheduled: fault_plan.feature.len(),
        feature_faults_fired: fault_plan
            .feature
            .iter()
            .filter(|fault| fault.fired)
            .count(),
        missing_feature_faults: fault_plan.missing_feature_faults(),
        graph_searches,
        filtered_searches,
        filtered_graph_searches,
        predicate_searches,
        hybrid_searches,
        text_documents_ingested,
        store_lexical_searches,
        store_hybrid_searches,
        phrase_searches,
        prefix_searches,
        fuzzy_searches,
        phonetic_encodes,
        snippets_built,
        hybrid_sealed_vector_documents,
        hybrid_lexical_documents,
        epoch_preparations,
        epoch_alias_switches,
        epoch_rollbacks,
        epoch_drops,
        rejected_dropped_epoch_rollbacks,
        coverage,
        violations,
        program_bytes,
        faults_bytes,
        violations_bytes,
        coverage_bytes: Vec::new(),
        oracle_bytes,
        controls_bytes,
        receipts_bytes,
        mutations_bytes,
        episode_bytes: Vec::new(),
        family_artifact_bytes,
        comparison_counts,
        comparison_pass_counts,
        comparison_outcome_counts,
        same_seed_clean_controls,
        integrated_feature_fault_receipts,
        expected_feature_fault_receipts,
    };
    outcome.coverage_bytes = artifacts.write_coverage(&outcome.coverage)?;
    if campaign == CampaignKind::VectorExecution {
        let coverage_alias =
            artifacts.write_family_artifact(campaign, "coverage.jsonl", &outcome.coverage_bytes)?;
        outcome
            .family_artifact_bytes
            .insert("coverage.jsonl".to_owned(), coverage_alias);
        let mut violations_alias = Vec::new();
        for violation in &outcome.violations {
            violations_alias.extend_from_slice(violation.json_for(campaign).as_bytes());
            violations_alias.push(b'\n');
        }
        let violations_alias =
            artifacts.write_family_artifact(campaign, "violations.jsonl", &violations_alias)?;
        outcome
            .family_artifact_bytes
            .insert("violations.jsonl".to_owned(), violations_alias);
    }
    if campaign != CampaignKind::Overall {
        let mut evidence_digests = BTreeMap::<String, String>::new();
        let family_evidence = outcome
            .family_artifact_bytes
            .values()
            .map(Vec::as_slice)
            .collect::<Vec<_>>();
        let mut operation_evidence = vec![
            outcome.program_bytes.as_slice(),
            outcome.controls_bytes.as_slice(),
        ];
        operation_evidence.extend(family_evidence.iter().copied());
        evidence_digests.insert(
            "operation_evidence".to_owned(),
            super::artifacts::evidence_digest(&operation_evidence),
        );
        evidence_digests.insert(
            "checker_evidence".to_owned(),
            super::artifacts::evidence_digest(&[&outcome.oracle_bytes]),
        );
        let mut fault_evidence = vec![
            outcome.faults_bytes.as_slice(),
            outcome.receipts_bytes.as_slice(),
            outcome.mutations_bytes.as_slice(),
        ];
        fault_evidence.extend(family_evidence.iter().copied());
        evidence_digests.insert(
            "fault_evidence".to_owned(),
            super::artifacts::evidence_digest(&fault_evidence),
        );
        let family_oracle_attestation = match campaign {
            CampaignKind::StorageDurability => Some(storage_episode_oracle_attestation(
                &outcome,
                &oracle_records,
            )?),
            CampaignKind::VectorExecution => Some(vector_episode_oracle_attestation(
                &outcome,
                &oracle_records,
            )?),
            CampaignKind::IngestRetention => Some(ingest_episode_oracle_attestation(
                &outcome,
                &oracle_records,
            )?),
            _ => None,
        };
        let attestation = super::artifacts::EpisodeAttestation {
            comparison_counts: outcome.comparison_counts.clone(),
            comparison_outcome_counts: outcome.comparison_outcome_counts.clone(),
            same_seed_clean_controls: outcome.same_seed_clean_controls,
            integrated_feature_fault_receipts: outcome.integrated_feature_fault_receipts,
            expected_feature_fault_receipts: outcome.expected_feature_fault_receipts,
            evidence_digests,
            family_oracle_attestation,
        };
        episode_bytes = artifacts.write_episode_metadata(
            campaign,
            seed,
            profile,
            profile_override.is_some(),
            &reproduction,
            Some(&attestation),
        )?;
    }
    outcome.episode_bytes = episode_bytes;
    if campaign == CampaignKind::VectorExecution {
        let episode_summary = artifacts.write_family_artifact(
            campaign,
            "episode-summary.json",
            &outcome.episode_bytes,
        )?;
        outcome
            .family_artifact_bytes
            .insert("episode-summary.json".to_owned(), episode_summary);
    }
    Ok(outcome)
}

fn crash_after_coverage_counts(faults: &[FaultEvent]) -> BTreeMap<&'static str, usize> {
    let mut counts = BTreeMap::new();
    for content in faults.iter().filter(|event| {
        event.fired
            && event.layer == fault_vfs::Layer::Content
            && faults.iter().any(|crash| {
                crash.layer == fault_vfs::Layer::Crash && crash.op_index >= event.op_index
            })
    }) {
        let count = counts.entry(content.mode.key()).or_insert(0_usize);
        *count = count.saturating_add(1);
    }
    counts
}

fn render_family_artifact(name: &str, records: &[String]) -> Result<Vec<u8>, String> {
    if name == "metadata-fixture.json" {
        let mut bytes = b"{\"campaign\":\"metadata-filter-planner\",\"operations\":[".to_vec();
        for (index, record) in records.iter().enumerate() {
            if index != 0 {
                bytes.push(b',');
            }
            bytes.extend_from_slice(record.as_bytes());
        }
        bytes.extend_from_slice(b"]}\n");
        return Ok(bytes);
    }
    if name == "fixture.json" {
        let campaign = records
            .first()
            .ok_or_else(|| "family fixture.json requires at least one record".to_owned())
            .and_then(|record| {
                zeppelin_embed_bench::harness_json::from_str::<
                    zeppelin_embed_bench::harness_json::Value,
                >(record)
                .map_err(|error| format!("parse fixture.json record: {error}"))
            })?
            .get("campaign")
            .and_then(zeppelin_embed_bench::harness_json::Value::as_str)
            .ok_or_else(|| "family fixture.json record omitted campaign".to_owned())?
            .to_owned();
        let mut bytes = format!(
            "{{\"campaign\":\"{}\",\"operations\":[",
            json_escape(&campaign)
        )
        .into_bytes();
        for (index, record) in records.iter().enumerate() {
            if index != 0 {
                bytes.push(b',');
            }
            bytes.extend_from_slice(record.as_bytes());
        }
        bytes.extend_from_slice(b"]}\n");
        return Ok(bytes);
    }
    if name.ends_with(".json") && !name.ends_with(".jsonl") && records.len() != 1 {
        return Err(format!(
            "family JSON artifact {name} requires exactly one record, observed {}",
            records.len()
        ));
    }
    let mut bytes = Vec::new();
    for record in records {
        bytes.extend_from_slice(record.as_bytes());
        bytes.push(b'\n');
    }
    Ok(bytes)
}

fn record_successful_operation_coverage(coverage: &mut CoverageRegistry, op: &Op) {
    coverage.hit(format!("op.{}", op.kind()));
    match op {
        Op::Ingest { .. } | Op::Upsert { .. } | Op::Revise { .. } => {
            coverage.hit("store.text_ingest");
        }
        Op::Search { kind, .. } => coverage.hit(format!("search.{}", kind.key())),
        Op::PredicateSearch { predicate, .. } => {
            coverage.hit(format!("predicate.{}", predicate.key()));
        }
        Op::Crash { boundary, .. } => {
            coverage.hit("store.text_ingest");
            coverage.hit(format!("crash.boundary.{}", boundary.key()));
        }
        Op::Feature(operation) => {
            coverage.hit(format!(
                "campaign.op.{}.{}",
                operation.campaign().key(),
                operation.key()
            ));
            if operation.campaign() == CampaignKind::FfiBindings {
                coverage.hit("op.ffi_probe");
                // Only the Rust caller of the extern "C" surface actually ran.
                // C, Python, and Swift adapters do not exist in this harness
                // yet; their coverage keys stay missing until a real adapter
                // process earns them.
                coverage.hit("binding.language.rust");
            }
        }
        _ => {}
    }
}

struct CampaignOperationContext<'a> {
    selected_faults: &'a [super::campaign::FeatureFault],
    generic_fault: Option<&'a FaultEvent>,
    storage_episode: Option<&'a storage_adapter::StorageEpisodeFixtures>,
    hybrid_episode: Option<&'a hybrid_adapter::HybridEpisode>,
    seed: u64,
    profile: FaultProfile,
    op_index: usize,
    oracle_records: &'a mut Vec<OracleRecord>,
    control_records: &'a mut Vec<String>,
    mutation_records: &'a mut Vec<String>,
    family_artifact_records: &'a mut BTreeMap<&'static str, Vec<String>>,
    coverage: &'a mut CoverageRegistry,
    control_store: Option<&'a mut ControlStore>,
}

struct CampaignOperationOutcome {
    receipts: Vec<ProductionFeatureReceipt>,
    qualifying_controls: usize,
    generic_fault_event: Option<FaultEvent>,
    refused_comparison_counts: BTreeMap<String, u64>,
}

#[derive(Debug)]
enum ProductionFeatureReceipt {
    ValidatedStorage(StorageFaultReceipt),
    ValidatedIngestRetention(IngestRetentionFaultReceiptV1),
    ValidatedMetadataFeature(MetadataFeatureReceipt),
    ValidatedVectorFeature(VectorFaultReceipt),
    ValidatedGraph(graph_adapter::GraphFaultReceipt),
    ValidatedFts(fts_adapter::FtsFaultReceipt),
    ValidatedHybrid(hybrid_adapter::HybridFaultReceipt),
    ValidatedTier(tier_adapter::TierFaultReceipt),
    ValidatedLifecycle(lifecycle_adapter::LifecycleFaultReceipt),
    ValidatedDiagnostics(diagnostics_adapter::DiagnosticsFaultReceipt),
    ValidatedFfi(ffi_adapter::FfiFaultReceipt),
    MetadataExecution {
        operation: &'static str,
        receipt: MetadataExecutionReceipt,
    },
}

impl ProductionFeatureReceipt {
    fn campaign(&self) -> &str {
        match self {
            Self::ValidatedStorage(receipt) => receipt.campaign(),
            Self::ValidatedIngestRetention(receipt) => receipt.campaign(),
            Self::ValidatedMetadataFeature(receipt) => receipt.origin.campaign(),
            Self::ValidatedVectorFeature(receipt) => vector_campaign_key(receipt.campaign()),
            Self::ValidatedGraph(_) => "vamana-graph",
            Self::ValidatedFts(_) => "fts",
            Self::ValidatedHybrid(_) => "hybrid-fusion",
            Self::ValidatedTier(_) => "tiering-maintenance",
            Self::ValidatedLifecycle(_) => "lifecycle-accounting",
            Self::ValidatedDiagnostics(_) => "diagnostics-health",
            Self::ValidatedFfi(_) => "ffi-bindings",
            Self::MetadataExecution { .. } => "metadata-filter-planner",
        }
    }

    fn operation(&self) -> &str {
        match self {
            Self::ValidatedStorage(receipt) => receipt.operation(),
            Self::ValidatedIngestRetention(receipt) => match receipt.operation() {
                ProductIngestRetentionOperation::BatchCommit => "batch-commit",
                ProductIngestRetentionOperation::Seal => "seal",
                ProductIngestRetentionOperation::Retention => "retention",
                ProductIngestRetentionOperation::Purge => "purge",
            },
            Self::ValidatedMetadataFeature(receipt) => receipt.origin.operation(),
            Self::ValidatedVectorFeature(receipt) => vector_operation_key(receipt.operation()),
            Self::ValidatedGraph(receipt) => receipt.operation.key(),
            Self::ValidatedFts(receipt) => receipt.operation.key(),
            Self::ValidatedHybrid(receipt) => receipt.operation.key(),
            Self::ValidatedTier(receipt) => receipt.operation.key(),
            Self::ValidatedLifecycle(receipt) => receipt.operation.key(),
            Self::ValidatedDiagnostics(receipt) => receipt.operation.key(),
            Self::ValidatedFfi(receipt) => receipt.operation.key(),
            Self::MetadataExecution { operation, .. } => operation,
        }
    }

    fn fault(&self) -> Option<&str> {
        match self {
            Self::ValidatedStorage(receipt) => Some(receipt.fault()),
            Self::ValidatedIngestRetention(receipt) => Some(match receipt.fault() {
                ProductIngestRetentionFaultKind::PostAckRetry => "post-ack-retry",
                ProductIngestRetentionFaultKind::PartialBatchAppend => "partial-batch-append",
                ProductIngestRetentionFaultKind::SealCancellation => "seal-cancellation",
                ProductIngestRetentionFaultKind::RetentionClockBoundary => {
                    "retention-clock-boundary"
                }
                ProductIngestRetentionFaultKind::PurgeUnlinkError => "purge-unlink-error",
                ProductIngestRetentionFaultKind::PurgeCrashBoundary => "purge-crash-boundary",
            }),
            Self::ValidatedMetadataFeature(receipt) => Some(receipt.origin.fault()),
            Self::ValidatedVectorFeature(receipt) => Some(vector_fault_key(receipt.fault())),
            Self::ValidatedGraph(receipt) => Some(receipt.fault.key()),
            Self::ValidatedFts(receipt) => Some(receipt.fault.key()),
            Self::ValidatedHybrid(receipt) => Some(receipt.fault.key()),
            Self::ValidatedTier(receipt) => Some(receipt.fault.key()),
            Self::ValidatedLifecycle(receipt) => Some(receipt.fault.key()),
            Self::ValidatedDiagnostics(receipt) => Some(receipt.fault.key()),
            Self::ValidatedFfi(receipt) => Some(receipt.fault.key()),
            Self::MetadataExecution { .. } => None,
        }
    }

    fn site(&self) -> &str {
        match self {
            Self::ValidatedStorage(receipt) => receipt.site(),
            Self::ValidatedIngestRetention(receipt) => match receipt.checkpoint() {
                IngestRetentionCheckpoint::IngestReplayNoWalAppend => "ingest.replay.no-wal-append",
                IngestRetentionCheckpoint::IngestCommitManyAppendError => {
                    "ingest.commit-many.append-error"
                }
                IngestRetentionCheckpoint::SealAfterSegmentWriteBeforeManifestCommit => {
                    "seal.after-segment-write.before-manifest-commit"
                }
                IngestRetentionCheckpoint::RetentionPolicyEvaluated => "retention.policy-evaluated",
                IngestRetentionCheckpoint::PurgeOldSegmentUnlinkError => {
                    "purge.old-segment-unlink.error"
                }
                IngestRetentionCheckpoint::PurgeAfterDurableIntentBeforeRewrite => {
                    "purge.after-durable-intent.before-rewrite"
                }
            },
            Self::ValidatedMetadataFeature(receipt) => receipt.origin.site(),
            Self::ValidatedVectorFeature(receipt) => vector_site_key(receipt.site()),
            Self::ValidatedGraph(receipt) => receipt.site,
            Self::ValidatedFts(receipt) => receipt.site,
            Self::ValidatedHybrid(receipt) => receipt.site,
            Self::ValidatedTier(receipt) => receipt.site,
            Self::ValidatedLifecycle(receipt) => receipt.site,
            Self::ValidatedDiagnostics(receipt) => receipt.site,
            Self::ValidatedFfi(receipt) => receipt.site,
            Self::MetadataExecution { .. } => "planner.exec.execution-receipt",
        }
    }

    fn cardinality(&self) -> Option<u32> {
        match self {
            Self::ValidatedStorage(receipt) => Some(receipt.cardinality()),
            Self::ValidatedIngestRetention(receipt) => Some(u32::from(receipt.cardinality())),
            Self::ValidatedMetadataFeature(receipt) => Some(receipt.origin.cardinality()),
            Self::ValidatedVectorFeature(receipt) => Some(u32::from(receipt.cardinality())),
            Self::ValidatedGraph(receipt) => Some(u32::from(receipt.cardinality)),
            Self::ValidatedFts(receipt) => Some(u32::from(receipt.cardinality)),
            Self::ValidatedHybrid(receipt) => Some(u32::from(receipt.cardinality)),
            Self::ValidatedTier(receipt) => Some(u32::from(receipt.cardinality)),
            Self::ValidatedLifecycle(receipt) => Some(u32::from(receipt.cardinality)),
            Self::ValidatedDiagnostics(receipt) => Some(u32::from(receipt.cardinality)),
            Self::ValidatedFfi(receipt) => Some(u32::from(receipt.cardinality)),
            Self::MetadataExecution { .. } => None,
        }
    }
}

fn storage_product_receipt_plan_json(plan: &StorageFaultPlan) -> String {
    let optional_u64 =
        |value: Option<u64>| value.map_or_else(|| "null".to_owned(), |value| value.to_string());
    let optional_u32 =
        |value: Option<u32>| value.map_or_else(|| "null".to_owned(), |value| value.to_string());
    let optional_u16 =
        |value: Option<u16>| value.map_or_else(|| "null".to_owned(), |value| value.to_string());
    let segment = plan.segment().map_or_else(
        || "null".to_owned(),
        |segment| format!("\"{}\"", evidence_hex(segment.as_bytes())),
    );
    format!(
        "{{\"op_index\":{},\"artifact\":\"{}\",\"offset\":{},\"segment\":{segment},\"region_kind\":{},\"chunk\":{}}}",
        plan.op_index(),
        json_escape(plan.artifact()),
        optional_u64(plan.offset()),
        optional_u16(plan.region_kind()),
        optional_u32(plan.chunk()),
    )
}

pub(crate) fn storage_receipt_evidence_digest(
    record: &zeppelin_embed_bench::harness_json::Value,
) -> Result<String, String> {
    let mut payload = record.clone();
    payload
        .as_object_mut()
        .ok_or_else(|| "storage production receipt is not an object".to_owned())?
        .remove("receipt_digest");
    let bytes = zeppelin_embed_bench::harness_json::to_vec(&payload)
        .map_err(|error| format!("serialize storage production receipt evidence: {error}"))?;
    Ok(super::artifacts::evidence_digest(&[
        b"storage-production-receipt-v1",
        &bytes,
    ]))
}

fn production_receipt_json(receipt: &ProductionFeatureReceipt) -> String {
    match receipt {
        ProductionFeatureReceipt::ValidatedStorage(receipt) => {
            let converted = storage_adapter::receipt_observed_from_product(receipt)
                .unwrap_or_else(|error| panic!("validated storage receipt conversion: {error}"));
            assert_eq!(converted.cardinality, receipt.cardinality());
            let plan = storage_product_receipt_plan_json(receipt.plan());
            let observed = storage_receipt_effect_json(&converted.value.effect);
            let mut record = zeppelin_embed_bench::harness_json::json!({
                "campaign": receipt.campaign(),
                "operation": receipt.operation(),
                "fault": receipt.fault(),
                "site": receipt.site(),
                "cardinality": receipt.cardinality(),
                "plan": zeppelin_embed_bench::harness_json::from_str::<
                    zeppelin_embed_bench::harness_json::Value,
                >(&plan)
                .expect("typed storage receipt plan JSON"),
                "observed": zeppelin_embed_bench::harness_json::from_str::<
                    zeppelin_embed_bench::harness_json::Value,
                >(&observed)
                .expect("typed storage receipt observation JSON"),
            });
            let digest = storage_receipt_evidence_digest(&record)
                .expect("typed storage receipt evidence digest");
            record
                .as_object_mut()
                .expect("storage receipt object")
                .insert(
                    "receipt_digest".to_owned(),
                    zeppelin_embed_bench::harness_json::Value::String(digest),
                );
            record.to_string()
        }
        ProductionFeatureReceipt::ValidatedIngestRetention(receipt) => {
            let base = format!(
                "{{\"campaign\":\"{}\",\"operation\":\"{}\",\"fault\":\"{}\",\"site\":\"{}\",\"cardinality\":{},\"invocation_id\":{},\"effect\":{}}}",
                receipt.campaign(),
                match receipt.operation() {
                    ProductIngestRetentionOperation::BatchCommit => "batch-commit",
                    ProductIngestRetentionOperation::Seal => "seal",
                    ProductIngestRetentionOperation::Retention => "retention",
                    ProductIngestRetentionOperation::Purge => "purge",
                },
                match receipt.fault() {
                    ProductIngestRetentionFaultKind::PostAckRetry => "post-ack-retry",
                    ProductIngestRetentionFaultKind::PartialBatchAppend => "partial-batch-append",
                    ProductIngestRetentionFaultKind::SealCancellation => "seal-cancellation",
                    ProductIngestRetentionFaultKind::RetentionClockBoundary => {
                        "retention-clock-boundary"
                    }
                    ProductIngestRetentionFaultKind::PurgeUnlinkError => "purge-unlink-error",
                    ProductIngestRetentionFaultKind::PurgeCrashBoundary => "purge-crash-boundary",
                },
                match receipt.checkpoint() {
                    IngestRetentionCheckpoint::IngestReplayNoWalAppend => {
                        "ingest.replay.no-wal-append"
                    }
                    IngestRetentionCheckpoint::IngestCommitManyAppendError => {
                        "ingest.commit-many.append-error"
                    }
                    IngestRetentionCheckpoint::SealAfterSegmentWriteBeforeManifestCommit => {
                        "seal.after-segment-write.before-manifest-commit"
                    }
                    IngestRetentionCheckpoint::RetentionPolicyEvaluated => {
                        "retention.policy-evaluated"
                    }
                    IngestRetentionCheckpoint::PurgeOldSegmentUnlinkError => {
                        "purge.old-segment-unlink.error"
                    }
                    IngestRetentionCheckpoint::PurgeAfterDurableIntentBeforeRewrite => {
                        "purge.after-durable-intent.before-rewrite"
                    }
                },
                receipt.cardinality(),
                receipt.invocation_id(),
                match receipt.effect() {
                    IngestRetentionFaultEffect::PostAckRetry {
                        batch_count,
                        replay_count,
                        returned_seq,
                        returned_generation,
                        wal_records_appended,
                        generation_delta,
                        active_published,
                    } => format!(
                        "{{\"kind\":\"post-ack-retry\",\"batch_count\":{batch_count},\"replay_count\":{replay_count},\"returned_seq\":{returned_seq},\"returned_generation\":{returned_generation},\"wal_records_appended\":{wal_records_appended},\"generation_delta\":{generation_delta},\"active_published\":{active_published}}}"
                    ),
                    IngestRetentionFaultEffect::PartialBatchAppend {
                        submitted_count,
                        changed_records,
                        encoded_bytes,
                        prefix_bytes,
                        io_kind,
                        detail,
                        active_published,
                        generation_delta,
                    } => format!(
                        "{{\"kind\":\"partial-batch-append\",\"submitted_count\":{submitted_count},\"changed_records\":{changed_records},\"encoded_bytes\":{encoded_bytes},\"prefix_bytes\":{prefix_bytes},\"io_kind\":\"{}\",\"detail\":\"{}\",\"active_published\":{active_published},\"generation_delta\":{generation_delta}}}",
                        match io_kind {
                            IngestRetentionIoKind::Other => "other",
                        },
                        json_escape(detail),
                    ),
                    IngestRetentionFaultEffect::SealCancellation {
                        active_rows,
                        absorbed_wal_end,
                        candidate_segment,
                        manifest_committed,
                        temporary_segment_removed,
                        generation_delta,
                    } => format!(
                        "{{\"kind\":\"seal-cancellation\",\"active_rows\":{active_rows},\"absorbed_wal_end\":{absorbed_wal_end},\"candidate_segment\":\"{}\",\"manifest_committed\":{manifest_committed},\"temporary_segment_removed\":{temporary_segment_removed},\"generation_delta\":{generation_delta}}}",
                        evidence_hex(candidate_segment),
                    ),
                    IngestRetentionFaultEffect::RetentionClockBoundary {
                        supplied_now,
                        window,
                        cutoff,
                        range_start,
                        range_end,
                        report_generation,
                        dropped_count,
                        straddler_count,
                        manifest_committed,
                    } => format!(
                        "{{\"kind\":\"retention-clock-boundary\",\"supplied_now\":{supplied_now},\"window\":{window},\"cutoff\":{cutoff},\"range_start\":{range_start},\"range_end\":{range_end},\"report_generation\":{report_generation},\"dropped_count\":{dropped_count},\"straddler_count\":{straddler_count},\"manifest_committed\":{manifest_committed}}}"
                    ),
                    IngestRetentionFaultEffect::PurgeUnlinkError {
                        original_segment,
                        replacement_segment,
                        old_file_name,
                        io_kind,
                        replacement_manifest_committed,
                        intent_present,
                        old_path_linked,
                    } => format!(
                        "{{\"kind\":\"purge-unlink-error\",\"original_segment\":\"{}\",\"replacement_segment\":\"{}\",\"old_file_name\":\"{}\",\"io_kind\":\"{}\",\"replacement_manifest_committed\":{replacement_manifest_committed},\"intent_present\":{intent_present},\"old_path_linked\":{old_path_linked}}}",
                        evidence_hex(original_segment),
                        evidence_hex(replacement_segment),
                        json_escape(old_file_name),
                        match io_kind {
                            IngestRetentionIoKind::Other => "other",
                        },
                    ),
                    IngestRetentionFaultEffect::PurgeCrashBoundary {
                        target_ids,
                        token_id,
                        intent_file_name,
                        intent_durable,
                        artifact_rewrites,
                        child_aborted,
                    } => {
                        let target_ids = target_ids
                            .iter()
                            .map(|target_id| format!("\"{target_id}\""))
                            .collect::<Vec<_>>()
                            .join(",");
                        format!(
                            "{{\"kind\":\"purge-crash-boundary\",\"target_ids\":[{target_ids}],\"token_id\":{token_id},\"intent_file_name\":\"{}\",\"intent_durable\":{intent_durable},\"artifact_rewrites\":{artifact_rewrites},\"child_aborted\":{child_aborted}}}",
                            json_escape(intent_file_name),
                        )
                    }
                },
            );
            let mut record: zeppelin_embed_bench::harness_json::Value =
                zeppelin_embed_bench::harness_json::from_str(&base)
                    .expect("typed ingest-retention receipt JSON");
            let checksum = ingest_retention_receipt_checksum(&record)
                .expect("typed ingest-retention receipt checksum");
            record
                .as_object_mut()
                .expect("ingest-retention receipt object")
                .insert(
                    "receipt_checksum".to_owned(),
                    zeppelin_embed_bench::harness_json::Value::String(checksum),
                );
            record.to_string()
        }
        ProductionFeatureReceipt::ValidatedMetadataFeature(receipt) => format!(
            "{{\"campaign\":\"{}\",\"operation\":\"{}\",\"fault\":\"{}\",\"site\":\"{}\",\"cardinality\":{},\"query_id\":{},\"effect\":\"{}\",\"detail\":{}}}",
            json_escape(receipt.origin.campaign()),
            json_escape(receipt.origin.operation()),
            json_escape(receipt.origin.fault()),
            json_escape(receipt.origin.site()),
            receipt.origin.cardinality(),
            receipt.query_id,
            json_escape(receipt.origin.effect()),
            metadata_product_feature_detail_json(&receipt.detail),
        ),
        ProductionFeatureReceipt::ValidatedVectorFeature(receipt) => format!(
            "{{\"campaign\":\"{}\",\"operation\":\"{}\",\"fault\":\"{}\",\"site\":\"{}\",\"cardinality\":{},\"seed_case_id\":{},\"effect\":{},\"result_published\":{}}}",
            vector_campaign_key(receipt.campaign()),
            vector_operation_key(receipt.operation()),
            vector_fault_key(receipt.fault()),
            vector_site_key(receipt.site()),
            receipt.cardinality(),
            receipt.seed_case_id(),
            vector_effect_json(receipt.effect()),
            receipt.result_published(),
        ),
        ProductionFeatureReceipt::ValidatedGraph(receipt) => format!(
            "{{\"campaign\":\"vamana-graph\",\"operation\":\"{}\",\"fault\":\"{}\",\"site\":\"{}\",\"cardinality\":{}}}",
            receipt.operation.key(),
            receipt.fault.key(),
            receipt.site,
            receipt.cardinality,
        ),
        ProductionFeatureReceipt::ValidatedFts(receipt) => format!(
            "{{\"campaign\":\"fts\",\"operation\":\"{}\",\"fault\":\"{}\",\"site\":\"{}\",\"cardinality\":{}}}",
            receipt.operation.key(),
            receipt.fault.key(),
            receipt.site,
            receipt.cardinality,
        ),
        ProductionFeatureReceipt::ValidatedHybrid(receipt) => format!(
            "{{\"campaign\":\"hybrid-fusion\",\"operation\":\"{}\",\"fault\":\"{}\",\"site\":\"{}\",\"cardinality\":{}}}",
            receipt.operation.key(),
            receipt.fault.key(),
            receipt.site,
            receipt.cardinality,
        ),
        ProductionFeatureReceipt::ValidatedTier(receipt) => format!(
            "{{\"campaign\":\"tiering-maintenance\",\"operation\":\"{}\",\"fault\":\"{}\",\"site\":\"{}\",\"cardinality\":{}}}",
            receipt.operation.key(),
            receipt.fault.key(),
            receipt.site,
            receipt.cardinality,
        ),
        ProductionFeatureReceipt::ValidatedLifecycle(receipt) => format!(
            "{{\"campaign\":\"lifecycle-accounting\",\"operation\":\"{}\",\"fault\":\"{}\",\"site\":\"{}\",\"cardinality\":{}}}",
            receipt.operation.key(),
            receipt.fault.key(),
            receipt.site,
            receipt.cardinality,
        ),
        ProductionFeatureReceipt::ValidatedDiagnostics(receipt) => format!(
            "{{\"campaign\":\"diagnostics-health\",\"operation\":\"{}\",\"fault\":\"{}\",\"site\":\"{}\",\"cardinality\":{}}}",
            receipt.operation.key(),
            receipt.fault.key(),
            receipt.site,
            receipt.cardinality,
        ),
        ProductionFeatureReceipt::ValidatedFfi(receipt) => format!(
            "{{\"campaign\":\"ffi-bindings\",\"operation\":\"{}\",\"fault\":\"{}\",\"site\":\"{}\",\"cardinality\":{}}}",
            receipt.operation.key(),
            receipt.fault.key(),
            receipt.site,
            receipt.cardinality,
        ),
        ProductionFeatureReceipt::MetadataExecution { operation, receipt } => format!(
            "{{\"campaign\":\"metadata-filter-planner\",\"operation\":\"{}\",\"site\":\"planner.exec.execution-receipt\",\"query_id\":{},\"receipt\":{}}}",
            json_escape(operation),
            receipt.query_id,
            metadata_product_execution_receipt_json(receipt),
        ),
    }
}

pub(crate) fn ingest_retention_receipt_json(receipt: &IngestRetentionFaultReceiptV1) -> String {
    production_receipt_json(&ProductionFeatureReceipt::ValidatedIngestRetention(
        receipt.clone(),
    ))
}

pub(crate) fn ingest_retention_receipt_checksum(
    record: &zeppelin_embed_bench::harness_json::Value,
) -> Result<String, String> {
    let mut canonical = record.clone();
    canonical
        .as_object_mut()
        .ok_or_else(|| "ingest-retention receipt is not an object".to_owned())?
        .remove("receipt_checksum");
    let bytes = zeppelin_embed_bench::harness_json::to_vec(&canonical)
        .map_err(|error| format!("serialize ingest-retention receipt checksum input: {error}"))?;
    Ok(super::artifacts::evidence_digest(&[&bytes]))
}

fn metadata_product_provenance_json(provenance: &MetadataDecodeProvenance) -> String {
    match provenance {
        MetadataDecodeProvenance::ColumnsPresenceTail {
            column_id,
            row_count,
            byte_offset,
            observed_byte,
            allowed_mask,
        } => format!(
            "{{\"kind\":\"columns-presence-tail\",\"column_id\":{column_id},\"row_count\":{row_count},\"byte_offset\":{byte_offset},\"observed_byte\":{observed_byte},\"allowed_mask\":{allowed_mask}}}"
        ),
        MetadataDecodeProvenance::ColumnsDictionaryCode {
            column_id,
            row,
            byte_offset,
            code,
            dictionary_cardinality,
        } => format!(
            "{{\"kind\":\"columns-dictionary-code\",\"column_id\":{column_id},\"row\":{row},\"byte_offset\":{byte_offset},\"code\":{code},\"dictionary_cardinality\":{dictionary_cardinality}}}"
        ),
        MetadataDecodeProvenance::ColumnsRawStringLength {
            column_id,
            row,
            byte_offset,
            declared_bytes,
            available_bytes,
        } => format!(
            "{{\"kind\":\"columns-raw-string-length\",\"column_id\":{column_id},\"row\":{row},\"byte_offset\":{byte_offset},\"declared_bytes\":{declared_bytes},\"available_bytes\":{available_bytes}}}"
        ),
        MetadataDecodeProvenance::AliveBitmapTruncation {
            row_count,
            byte_offset,
            declared_bytes,
            observed_bytes,
        } => format!(
            "{{\"kind\":\"alive-bitmap-truncation\",\"row_count\":{row_count},\"byte_offset\":{byte_offset},\"declared_bytes\":{declared_bytes},\"observed_bytes\":{observed_bytes}}}"
        ),
    }
}

fn metadata_product_feature_detail_json(detail: &MetadataFeatureDetail) -> String {
    match detail {
        MetadataFeatureDetail::ColumnDecodeRefused {
            source,
            field_class,
            byte_offset,
            error_class,
            provenance,
        } => format!(
            "{{\"kind\":\"column-decode-refused\",\"source\":\"{}\",\"field_class\":\"{}\",\"byte_offset\":{byte_offset},\"error_class\":\"{}\",\"provenance\":{}}}",
            json_escape(&metadata_source_label(*source)),
            json_escape(field_class),
            json_escape(error_class),
            metadata_product_provenance_json(provenance),
        ),
        MetadataFeatureDetail::AliveBitmapTruncationRefused {
            source,
            declared_rows,
            declared_bytes,
            observed_bytes,
            byte_offset,
            error_class,
            provenance,
        } => format!(
            "{{\"kind\":\"alive-bitmap-truncation-refused\",\"source\":\"{}\",\"declared_rows\":{declared_rows},\"declared_bytes\":{declared_bytes},\"observed_bytes\":{observed_bytes},\"byte_offset\":{byte_offset},\"error_class\":\"{}\",\"provenance\":{}}}",
            json_escape(&metadata_source_label(*source)),
            json_escape(error_class),
            metadata_product_provenance_json(provenance),
        ),
        MetadataFeatureDetail::SelectivityBoundaryChosen {
            source,
            cardinality,
            threshold,
            branch,
        } => format!(
            "{{\"kind\":\"selectivity-boundary-chosen\",\"source\":\"{}\",\"filter_cardinality\":{cardinality},\"threshold\":{threshold},\"branch\":\"{}\"}}",
            json_escape(&metadata_source_label(*source)),
            metadata_product_branch_key(*branch),
        ),
        MetadataFeatureDetail::VisitedBudgetFallback {
            source,
            visited,
            budget,
            filter_cardinality,
            exact_rows_examined,
            returned,
            reason,
        } => format!(
            "{{\"kind\":\"visited-budget-fallback\",\"source\":\"{}\",\"visited\":{visited},\"budget\":{budget},\"filter_cardinality\":{filter_cardinality},\"exact_rows_examined\":{exact_rows_examined},\"returned\":{returned},\"reason\":\"{}\"}}",
            json_escape(&metadata_source_label(*source)),
            metadata_product_fallback_key(*reason),
        ),
    }
}

fn metadata_product_branch_key(branch: SegmentBranch) -> &'static str {
    match branch {
        SegmentBranch::Pruned => "pruned",
        SegmentBranch::ExactAllowList => "exact-allow-list",
        SegmentBranch::MaskedScan => "masked-scan",
        SegmentBranch::Graph => "graph",
        SegmentBranch::FilteredGraph => "filtered-graph",
        SegmentBranch::GraphExactFallback => "graph-exact-fallback",
    }
}

fn metadata_product_fallback_key(fallback: PlanFallback) -> &'static str {
    match fallback {
        PlanFallback::None => "none",
        PlanFallback::EfWidened => "ef-widened",
        PlanFallback::VisitedBudget => "visited-budget",
        PlanFallback::CandidateShortfall => "candidate-shortfall",
    }
}

fn metadata_product_execution_receipt_json(receipt: &MetadataExecutionReceipt) -> String {
    let optional =
        |value: Option<usize>| value.map_or_else(|| "null".to_owned(), |value| value.to_string());
    format!(
        "{{\"query_id\":{},\"source\":\"{}\",\"row_count\":{},\"filter_cardinality\":{},\"branch\":\"{}\",\"fallback\":\"{}\",\"rows_examined\":{},\"allowed_rows_examined\":{},\"vectors_scored\":{},\"graph_nodes_visited\":{},\"exact_fallback_rows_examined\":{},\"returned_candidates\":{},\"ef_effective\":{},\"visited_budget\":{},\"sealed\":{}}}",
        receipt.query_id,
        json_escape(&metadata_source_label(receipt.source)),
        receipt.row_count,
        receipt.filter_cardinality,
        metadata_product_branch_key(receipt.branch),
        metadata_product_fallback_key(receipt.fallback),
        receipt.rows_examined,
        receipt.allowed_rows_examined,
        receipt.vectors_scored,
        receipt.graph_nodes_visited,
        receipt.exact_fallback_rows_examined,
        receipt.returned_candidates,
        optional(receipt.ef_effective),
        optional(receipt.visited_budget),
        receipt.sealed,
    )
}

fn run_campaign_operation_with_clean_control(
    operation: super::campaign::FeatureOperation,
    context: CampaignOperationContext<'_>,
) -> Result<CampaignOperationOutcome, String> {
    let CampaignOperationContext {
        selected_faults,
        generic_fault,
        storage_episode,
        hybrid_episode,
        seed,
        profile,
        op_index,
        oracle_records,
        control_records,
        mutation_records,
        family_artifact_records,
        coverage,
        control_store: _,
    } = context;
    let uses_shared_control = matches!(
        operation,
        super::campaign::FeatureOperation::Graph(_)
            | super::campaign::FeatureOperation::Fts(_)
            | super::campaign::FeatureOperation::Tiering(_)
            | super::campaign::FeatureOperation::Lifecycle(_)
            | super::campaign::FeatureOperation::Diagnostics(_)
            | super::campaign::FeatureOperation::Ffi(_)
    );
    if !uses_shared_control {
        let control_start = control_records.len();
        let receipts = run_campaign_operation(
            operation,
            CampaignOperationContext {
                selected_faults,
                generic_fault,
                storage_episode,
                hybrid_episode,
                seed,
                profile,
                op_index,
                oracle_records,
                control_records,
                mutation_records,
                family_artifact_records,
                coverage,
                control_store: None,
            },
        )?;
        let records = control_records
            .get(control_start..)
            .unwrap_or_default()
            .to_vec();
        let qualifying_controls = if operation.campaign() == CampaignKind::MetadataFilterPlanner {
            records
                .iter()
                .filter(|record| {
                    record.contains("\"fault\":") && !record.contains("\"fault\":null")
                })
                .count()
        } else {
            records.len()
        };
        return Ok(CampaignOperationOutcome {
            receipts,
            qualifying_controls,
            generic_fault_event: None,
            refused_comparison_counts: BTreeMap::new(),
        });
    }

    let open_options = match operation {
        super::campaign::FeatureOperation::Graph(_) => graph_adapter::control_open_options(seed),
        super::campaign::FeatureOperation::Tiering(_) => tier_adapter::control_open_options(seed),
        super::campaign::FeatureOperation::Lifecycle(
            super::campaign::LifecycleOperation::CloseDrain,
        ) => OpenOptions::default().with_reader_drain_timeout(Duration::ZERO),
        _ => OpenOptions::default(),
    };
    let fixture = FrozenStoreFixture::for_control(open_options, generic_fault.cloned(), op_index)?;
    let mut clean_oracle_records = Vec::new();
    let mut clean_control_records = Vec::new();
    let mut clean_mutation_records = Vec::new();
    let mut clean_family_artifact_records = BTreeMap::new();
    let mut clean_coverage = CoverageRegistry::default();
    #[derive(Debug)]
    struct FamilyLegResult {
        receipts: Vec<ProductionFeatureReceipt>,
        oracle_passed: bool,
    }
    let control = run_with_clean_control(
        &fixture,
        |store| {
            let receipts = run_campaign_operation(
                operation,
                CampaignOperationContext {
                    selected_faults: &[],
                    generic_fault: None,
                    storage_episode,
                    hybrid_episode,
                    seed,
                    profile,
                    op_index,
                    oracle_records: &mut clean_oracle_records,
                    control_records: &mut clean_control_records,
                    mutation_records: &mut clean_mutation_records,
                    family_artifact_records: &mut clean_family_artifact_records,
                    coverage: &mut clean_coverage,
                    control_store: Some(store),
                },
            )?;
            Ok(FamilyLegResult {
                receipts,
                oracle_passed: !clean_oracle_records.is_empty()
                    && clean_oracle_records.iter().all(|record| record.passed),
            })
        },
        |store| {
            let record_start = oracle_records.len();
            let receipts = run_campaign_operation(
                operation,
                CampaignOperationContext {
                    selected_faults,
                    generic_fault,
                    storage_episode,
                    hybrid_episode,
                    seed,
                    profile,
                    op_index,
                    oracle_records,
                    control_records,
                    mutation_records,
                    family_artifact_records,
                    coverage,
                    control_store: Some(store),
                },
            )?;
            Ok(FamilyLegResult {
                receipts,
                oracle_passed: oracle_records.get(record_start..).is_some_and(|records| {
                    !records.is_empty() && records.iter().all(|record| record.passed)
                }),
            })
        },
        |result| {
            if result.oracle_passed {
                Ok(())
            } else {
                Err("family oracle disagreed".to_owned())
            }
        },
    )?;
    if !control.same_seed_control_passed {
        let detail = clean_oracle_records
            .iter()
            .find(|record| !record.passed)
            .map_or_else(
                || "emitted no family oracle record".to_owned(),
                |record| format!("disagreed with {}: {}", record.checker_id, record.detail),
            );
        return Err(format!(
            "clean control for {}/{} {detail}",
            operation.campaign().key(),
            operation.key()
        ));
    }
    let generic_fault_event = control.fault_event.clone();
    let (receipts, refused_comparison_counts) = match control.faulted? {
        FaultedLeg::Observed(result) => (result.receipts, BTreeMap::new()),
        FaultedLeg::Refused { stage, error } => {
            let fault = control
                .fault_event
                .as_ref()
                .map_or("null".to_owned(), |event| {
                    format!("\"{}\"", json_escape(&event.id))
                });
            control_records.push(format!(
                "{{\"campaign\":\"{}\",\"operation\":\"{}\",\"seed\":{seed},\"fault\":{fault},\"typed_refusal\":true,\"stage\":\"{}\",\"error\":\"{}\"}}",
                operation.campaign().key(),
                operation.key(),
                json_escape(stage),
                json_escape(&error),
            ));
            let mut refused = BTreeMap::new();
            let comparison_multiplier = u64::try_from(selected_faults.len().max(1))
                .map_err(|_| "selected feature-fault count exceeds u64".to_owned())?;
            for record in &clean_oracle_records {
                let count = refused
                    .entry(format!("I{}", record.invariant))
                    .or_insert(0_u64);
                *count = count.saturating_add(comparison_multiplier);
            }
            (Vec::new(), refused)
        }
    };
    let control_start = control_records.len();
    for receipt in &receipts {
        if let Some(fault) = receipt.fault() {
            control_records.push(format!(
                "{{\"campaign\":\"{}\",\"operation\":\"{}\",\"seed\":{seed},\"fault\":\"{}\",\"qualifying_same_seed_control\":true,\"clean_control_passed\":{}}}",
                operation.campaign().key(),
                operation.key(),
                json_escape(fault),
                control.same_seed_control_passed,
            ));
        }
    }
    let qualifying_controls = control_records.len().saturating_sub(control_start);
    Ok(CampaignOperationOutcome {
        receipts,
        qualifying_controls,
        generic_fault_event,
        refused_comparison_counts,
    })
}

fn run_campaign_operation(
    operation: super::campaign::FeatureOperation,
    context: CampaignOperationContext<'_>,
) -> Result<Vec<ProductionFeatureReceipt>, String> {
    let CampaignOperationContext {
        selected_faults,
        generic_fault,
        storage_episode,
        hybrid_episode,
        seed,
        profile,
        op_index,
        oracle_records,
        control_records,
        mutation_records,
        family_artifact_records,
        coverage,
        mut control_store,
    } = context;
    if let super::campaign::FeatureOperation::Storage(storage_operation) = operation {
        let episode = storage_episode.ok_or_else(|| {
            "storage campaign operation omitted its shared episode base".to_owned()
        })?;
        return run_storage_campaign_operation(
            storage_operation,
            selected_faults,
            episode,
            seed,
            profile,
            op_index,
            oracle_records,
            control_records,
            mutation_records,
            family_artifact_records,
            coverage,
        );
    }
    if let super::campaign::FeatureOperation::Vector(vector_operation) = operation {
        return run_vector_campaign_operation(
            vector_operation,
            selected_faults,
            generic_fault,
            seed,
            profile,
            op_index,
            oracle_records,
            control_records,
            mutation_records,
            family_artifact_records,
            coverage,
        );
    }
    if let super::campaign::FeatureOperation::Metadata(metadata_operation) = operation {
        return run_metadata_campaign_operation(
            metadata_operation,
            selected_faults,
            seed,
            profile,
            op_index,
            oracle_records,
            control_records,
            mutation_records,
            family_artifact_records,
            coverage,
        );
    }
    if let super::campaign::FeatureOperation::Ingest(ingest_operation) = operation {
        return run_ingest_campaign_operation(
            ingest_operation,
            selected_faults,
            seed,
            profile,
            op_index,
            oracle_records,
            control_records,
            family_artifact_records,
            coverage,
        );
    }
    if let super::campaign::FeatureOperation::Graph(graph_operation) = operation {
        return run_graph_campaign_operation(
            graph_operation,
            selected_faults,
            seed,
            oracle_records,
            control_records,
            coverage,
            control_store,
        );
    }
    if let super::campaign::FeatureOperation::Fts(fts_operation) = operation {
        return run_fts_campaign_operation(
            fts_operation,
            selected_faults,
            seed,
            oracle_records,
            control_records,
            coverage,
            control_store.as_deref_mut(),
        );
    }
    if let super::campaign::FeatureOperation::Hybrid(hybrid_operation) = operation {
        let episode = hybrid_episode
            .ok_or_else(|| "hybrid campaign operation omitted its shared episode".to_owned())?;
        return run_hybrid_campaign_operation(
            hybrid_operation,
            selected_faults,
            episode,
            seed,
            oracle_records,
            control_records,
            coverage,
        );
    }
    if let super::campaign::FeatureOperation::Tiering(tier_operation) = operation {
        return run_tier_campaign_operation(
            tier_operation,
            selected_faults,
            seed,
            oracle_records,
            control_records,
            coverage,
            control_store.as_deref_mut(),
        );
    }
    if let super::campaign::FeatureOperation::Lifecycle(lifecycle_operation) = operation {
        return run_lifecycle_campaign_operation(
            lifecycle_operation,
            selected_faults,
            seed,
            oracle_records,
            control_records,
            coverage,
            control_store.as_deref_mut(),
        );
    }
    if let super::campaign::FeatureOperation::Diagnostics(diagnostics_operation) = operation {
        return run_diagnostics_campaign_operation(
            diagnostics_operation,
            selected_faults,
            seed,
            oracle_records,
            control_records,
            coverage,
            control_store.as_deref_mut(),
        );
    }
    if let super::campaign::FeatureOperation::Ffi(ffi_operation) = operation {
        return run_ffi_campaign_operation(
            ffi_operation,
            selected_faults,
            seed,
            oracle_records,
            control_records,
            coverage,
            control_store,
        );
    }
    for fault in selected_faults {
        if fault.operation() != operation {
            return Err(format!(
                "feature fault {} targeted {}, but executed at {}",
                fault.key(),
                fault.operation().key(),
                operation.key(),
            ));
        }
    }
    let _ = (
        seed,
        profile,
        op_index,
        oracle_records,
        control_records,
        mutation_records,
        family_artifact_records,
        coverage,
    );
    Err(format!(
        "{}/{} has no exact family observation adapter; no invariant or fault receipt credit is allowed",
        operation.campaign().key(),
        operation.key(),
    ))
}

fn tier_operation_kind(
    operation: super::campaign::TieringOperation,
) -> tier_adapter::TierOperationKind {
    match operation {
        super::campaign::TieringOperation::Policy => tier_adapter::TierOperationKind::Policy,
        super::campaign::TieringOperation::Transition => {
            tier_adapter::TierOperationKind::Transition
        }
        super::campaign::TieringOperation::Budget => tier_adapter::TierOperationKind::Budget,
        super::campaign::TieringOperation::Publication => {
            tier_adapter::TierOperationKind::Publication
        }
    }
}

fn tier_fault_kind(
    fault: super::campaign::FeatureFault,
) -> Result<tier_adapter::TierFaultKind, String> {
    match fault {
        super::campaign::FeatureFault::TierBudgetExhaustion => {
            Ok(tier_adapter::TierFaultKind::BudgetExhaustion)
        }
        super::campaign::FeatureFault::TierCheckpointCorruption => {
            Ok(tier_adapter::TierFaultKind::CheckpointCorruption)
        }
        super::campaign::FeatureFault::TierRefinementCheckpointCorruption => {
            Ok(tier_adapter::TierFaultKind::RefinementCheckpointCorruption)
        }
        super::campaign::FeatureFault::TierRefinementRenumberCrash => {
            Ok(tier_adapter::TierFaultKind::RefinementRenumberCrash)
        }
        super::campaign::FeatureFault::TierRefinementAlphaRepruneCrash => {
            Ok(tier_adapter::TierFaultKind::RefinementAlphaRepruneCrash)
        }
        super::campaign::FeatureFault::TierRefinementSeedRefitCrash => {
            Ok(tier_adapter::TierFaultKind::RefinementSeedRefitCrash)
        }
        super::campaign::FeatureFault::TierRefinementNeighborReorderCrash => {
            Ok(tier_adapter::TierFaultKind::RefinementNeighborReorderCrash)
        }
        super::campaign::FeatureFault::TierStaleSource => {
            Ok(tier_adapter::TierFaultKind::StaleSource)
        }
        super::campaign::FeatureFault::TierEnospc => Ok(tier_adapter::TierFaultKind::Enospc),
        super::campaign::FeatureFault::TierProfileMismatch => {
            Ok(tier_adapter::TierFaultKind::ProfileMismatch)
        }
        super::campaign::FeatureFault::TierPublicationCrash => {
            Ok(tier_adapter::TierFaultKind::PublicationCrash)
        }
        other => Err(format!("{} is not a tiering fault", other.key())),
    }
}

fn tier_input_json(input: &tier_oracle::TierInput) -> String {
    let source_segment = input
        .source_segment
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!(
        "{{\"seed\":{},\"rows\":{},\"dims\":{},\"policy_threshold\":{},\"maintenance_threshold\":{},\"stride\":{},\"k\":{},\"source_generation\":{},\"source_segment\":\"{source_segment}\"}}",
        input.seed,
        input.rows,
        input.dims,
        input.policy_threshold,
        input.maintenance_threshold,
        input.stride,
        input.k,
        input.source_generation,
    )
}

fn tier_observed_json(observed: &tier_oracle::TierObserved) -> String {
    let escaped = format!("{observed:?}")
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    format!("{{\"seed_derived_typed_observation\":\"{escaped}\"}}")
}

fn run_tier_campaign_operation(
    operation: super::campaign::TieringOperation,
    selected_faults: &[super::campaign::FeatureFault],
    seed: u64,
    oracle_records: &mut Vec<OracleRecord>,
    control_records: &mut Vec<String>,
    coverage: &mut CoverageRegistry,
    mut control_store: Option<&mut ControlStore>,
) -> Result<Vec<ProductionFeatureReceipt>, String> {
    let faults = selected_faults
        .iter()
        .copied()
        .map(tier_fault_kind)
        .collect::<Result<Vec<_>, _>>()?;
    let cases = if faults.is_empty() {
        vec![None]
    } else {
        faults.into_iter().map(Some).collect()
    };
    let mut receipts = Vec::new();
    for (case_index, fault) in cases.into_iter().enumerate() {
        if case_index > 0
            && let Some(store) = control_store.as_deref_mut()
        {
            store.reset_to_frozen()?;
        }
        let case_identity = format!(
            "seed-{seed}-operation-{}-fault-{}",
            operation.key(),
            fault.map_or("none", tier_adapter::TierFaultKind::key)
        );
        let evidence = match control_store.as_deref_mut() {
            Some(store) => tier_adapter::run_tier_operation_on_store(
                store,
                tier_operation_kind(operation),
                seed,
                fault,
            )?,
            None => tier_adapter::run_tier_operation(tier_operation_kind(operation), seed, fault)?,
        };
        for invariant in evidence.invariants {
            let (id, checker, input, observed, result) = match invariant {
                tier_adapter::TierInvariantEvidence::I50 { input, observed } => {
                    let result = tier_oracle::compare_i50(&input, &observed);
                    (50, tier_oracle::I50_CHECKER_ID, input, observed, result)
                }
                tier_adapter::TierInvariantEvidence::I51 { input, observed } => {
                    let result = tier_oracle::compare_i51(&input, &observed);
                    (51, tier_oracle::I51_CHECKER_ID, input, observed, result)
                }
                tier_adapter::TierInvariantEvidence::I52 { input, observed } => {
                    let result = tier_oracle::compare_i52(&input, &observed);
                    (52, tier_oracle::I52_CHECKER_ID, input, observed, result)
                }
                tier_adapter::TierInvariantEvidence::I53 { input, observed } => {
                    let result = tier_oracle::compare_i53(&input, &observed);
                    (53, tier_oracle::I53_CHECKER_ID, input, observed, result)
                }
            };
            push_feature_json_record(
                id,
                checker,
                operation.key(),
                tier_input_json(&input),
                tier_observed_json(&observed),
                format!("public tier operation seed={seed}"),
                result,
                oracle_records,
                coverage,
            );
            oracle_records
                .last_mut()
                .expect("tiering comparison appended one oracle record")
                .case_identity = Some(case_identity.clone());
        }
        let _ = control_records;
        for receipt in &evidence.receipts {
            coverage.hit(receipt.site);
        }
        receipts.extend(
            evidence
                .receipts
                .into_iter()
                .map(ProductionFeatureReceipt::ValidatedTier),
        );
    }
    coverage.hit("search.auto");
    Ok(receipts)
}

fn lifecycle_operation_kind(
    operation: super::campaign::LifecycleOperation,
) -> lifecycle_adapter::LifecycleOperationKind {
    match operation {
        super::campaign::LifecycleOperation::Deadline => {
            lifecycle_adapter::LifecycleOperationKind::Deadline
        }
        super::campaign::LifecycleOperation::Cancellation => {
            lifecycle_adapter::LifecycleOperationKind::Cancellation
        }
        super::campaign::LifecycleOperation::CloseDrain => {
            lifecycle_adapter::LifecycleOperationKind::CloseDrain
        }
        super::campaign::LifecycleOperation::Locking => {
            lifecycle_adapter::LifecycleOperationKind::Locking
        }
        super::campaign::LifecycleOperation::Accounting => {
            lifecycle_adapter::LifecycleOperationKind::Accounting
        }
    }
}

fn lifecycle_fault_kind(
    fault: super::campaign::FeatureFault,
) -> Result<lifecycle_adapter::LifecycleFaultKind, String> {
    match fault {
        super::campaign::FeatureFault::LifecycleClockFreezeJump => {
            Ok(lifecycle_adapter::LifecycleFaultKind::ClockFreezeJump)
        }
        super::campaign::FeatureFault::LifecycleCancelAdmissionQuery => {
            Ok(lifecycle_adapter::LifecycleFaultKind::CancelAdmissionQuery)
        }
        super::campaign::FeatureFault::LifecycleCloseActiveQuery => {
            Ok(lifecycle_adapter::LifecycleFaultKind::CloseActiveQuery)
        }
        super::campaign::FeatureFault::LifecycleWorkerPanic => {
            Ok(lifecycle_adapter::LifecycleFaultKind::WorkerPanic)
        }
        super::campaign::FeatureFault::LifecycleLockContention => {
            Ok(lifecycle_adapter::LifecycleFaultKind::LockContention)
        }
        super::campaign::FeatureFault::LifecycleAllocationDenial => {
            Ok(lifecycle_adapter::LifecycleFaultKind::AllocationDenial)
        }
        other => Err(format!("{} is not a lifecycle fault", other.key())),
    }
}

fn lifecycle_input_json(input: &lifecycle_oracle::LifecycleInput) -> String {
    format!(
        "{{\"expected_active_queries_after\":{}}}",
        input.expected_active_queries_after
    )
}

fn lifecycle_observed_json(observed: &lifecycle_oracle::LifecycleObserved) -> String {
    format!(
        "{{\"deadline_timed_out_without_partial\":{},\"cancellation_without_partial\":{},\"close_cancelled_active_query\":{},\"post_close_refused\":{},\"second_writer_refused\":{},\"active_queries_after\":{},\"query_pool_bytes\":{},\"allocation_denied\":{}}}",
        observed.deadline_timed_out_without_partial,
        observed.cancellation_without_partial,
        observed.close_cancelled_active_query,
        observed.post_close_refused,
        observed.second_writer_refused,
        observed.active_queries_after,
        observed.query_pool_bytes,
        observed.allocation_denied,
    )
}

fn run_lifecycle_campaign_operation(
    operation: super::campaign::LifecycleOperation,
    selected_faults: &[super::campaign::FeatureFault],
    seed: u64,
    oracle_records: &mut Vec<OracleRecord>,
    control_records: &mut Vec<String>,
    coverage: &mut CoverageRegistry,
    mut control_store: Option<&mut ControlStore>,
) -> Result<Vec<ProductionFeatureReceipt>, String> {
    let faults = selected_faults
        .iter()
        .copied()
        .map(lifecycle_fault_kind)
        .collect::<Result<Vec<_>, _>>()?;
    let cases = if faults.is_empty() {
        vec![None]
    } else {
        faults.into_iter().map(Some).collect()
    };
    let mut receipts = Vec::new();
    for (case_index, fault) in cases.into_iter().enumerate() {
        if case_index > 0
            && let Some(store) = control_store.as_deref_mut()
        {
            store.reset_to_frozen()?;
        }
        let case_identity = format!(
            "seed-{seed}-operation-{}-fault-{}",
            operation.key(),
            fault.map_or("none", lifecycle_adapter::LifecycleFaultKind::key)
        );
        let evidence = match control_store.as_deref() {
            Some(store) => lifecycle_adapter::run_lifecycle_operation_on_store(
                store.store()?,
                store.path(),
                lifecycle_operation_kind(operation),
                fault,
            )?,
            None => lifecycle_adapter::run_lifecycle_operation(
                lifecycle_operation_kind(operation),
                fault,
            )?,
        };
        let (id, checker, input, observed, result) = match evidence.invariant {
            lifecycle_adapter::LifecycleInvariantEvidence::I54 { input, observed } => {
                let result = lifecycle_oracle::compare_i54(&input, &observed);
                (
                    54,
                    lifecycle_oracle::I54_CHECKER_ID,
                    input,
                    observed,
                    result,
                )
            }
            lifecycle_adapter::LifecycleInvariantEvidence::I55 { input, observed } => {
                let result = lifecycle_oracle::compare_i55(&input, &observed);
                (
                    55,
                    lifecycle_oracle::I55_CHECKER_ID,
                    input,
                    observed,
                    result,
                )
            }
            lifecycle_adapter::LifecycleInvariantEvidence::I56 { input, observed } => {
                let result = lifecycle_oracle::compare_i56(&input, &observed);
                (
                    56,
                    lifecycle_oracle::I56_CHECKER_ID,
                    input,
                    observed,
                    result,
                )
            }
            lifecycle_adapter::LifecycleInvariantEvidence::I57 { input, observed } => {
                let result = lifecycle_oracle::compare_i57(&input, &observed);
                (
                    57,
                    lifecycle_oracle::I57_CHECKER_ID,
                    input,
                    observed,
                    result,
                )
            }
            lifecycle_adapter::LifecycleInvariantEvidence::I58 { input, observed } => {
                let result = lifecycle_oracle::compare_i58(&input, &observed);
                (
                    58,
                    lifecycle_oracle::I58_CHECKER_ID,
                    input,
                    observed,
                    result,
                )
            }
        };
        push_feature_json_record(
            id,
            checker,
            operation.key(),
            lifecycle_input_json(&input),
            lifecycle_observed_json(&observed),
            format!("public lifecycle operation seed={seed}"),
            result,
            oracle_records,
            coverage,
        );
        oracle_records
            .last_mut()
            .expect("lifecycle comparison appended one oracle record")
            .case_identity = Some(case_identity);
        let _ = control_records;
        receipts.extend(
            evidence
                .receipts
                .into_iter()
                .map(ProductionFeatureReceipt::ValidatedLifecycle),
        );
    }
    Ok(receipts)
}

fn diagnostics_operation_kind(
    operation: super::campaign::DiagnosticsOperation,
) -> diagnostics_adapter::DiagnosticsOperationKind {
    match operation {
        super::campaign::DiagnosticsOperation::Health => {
            diagnostics_adapter::DiagnosticsOperationKind::Health
        }
        super::campaign::DiagnosticsOperation::SelfCheck => {
            diagnostics_adapter::DiagnosticsOperationKind::SelfCheck
        }
        super::campaign::DiagnosticsOperation::Recovery => {
            diagnostics_adapter::DiagnosticsOperationKind::Recovery
        }
    }
}

fn diagnostics_fault_kind(
    fault: super::campaign::FeatureFault,
) -> Result<diagnostics_adapter::DiagnosticsFaultKind, String> {
    match fault {
        super::campaign::FeatureFault::DiagnosticsCounterPlanMutation => {
            Ok(diagnostics_adapter::DiagnosticsFaultKind::CounterPlanMutation)
        }
        super::campaign::FeatureFault::DiagnosticsCorruptArtifact => {
            Ok(diagnostics_adapter::DiagnosticsFaultKind::CorruptArtifact)
        }
        super::campaign::FeatureFault::DiagnosticsStaleHealth => {
            Ok(diagnostics_adapter::DiagnosticsFaultKind::StaleHealth)
        }
        other => Err(format!("{} is not a diagnostics fault", other.key())),
    }
}

fn diagnostics_input_json(input: &diagnostics_oracle::DiagnosticsInput) -> String {
    format!(
        "{{\"expected_documents\":{},\"expect_corruption\":{}}}",
        input.expected_documents, input.expect_corruption
    )
}

fn diagnostics_observed_json(observed: &diagnostics_oracle::DiagnosticsObserved) -> String {
    format!(
        "{{\"pending_documents\":{},\"returned_matches_candidates\":{},\"counter_delta_matches_one_row\":{},\"self_check_healthy\":{},\"corruption_attributed\":{},\"recovery_cleared_fault\":{}}}",
        observed.pending_documents,
        observed.returned_matches_candidates,
        observed.counter_delta_matches_one_row,
        observed.self_check_healthy,
        observed.corruption_attributed,
        observed.recovery_cleared_fault,
    )
}

fn run_diagnostics_campaign_operation(
    operation: super::campaign::DiagnosticsOperation,
    selected_faults: &[super::campaign::FeatureFault],
    seed: u64,
    oracle_records: &mut Vec<OracleRecord>,
    control_records: &mut Vec<String>,
    coverage: &mut CoverageRegistry,
    mut control_store: Option<&mut ControlStore>,
) -> Result<Vec<ProductionFeatureReceipt>, String> {
    let faults = selected_faults
        .iter()
        .copied()
        .map(diagnostics_fault_kind)
        .collect::<Result<Vec<_>, _>>()?;
    let cases = if faults.is_empty() {
        vec![None]
    } else {
        faults.into_iter().map(Some).collect()
    };
    let mut receipts = Vec::new();
    for (case_index, fault) in cases.into_iter().enumerate() {
        if case_index > 0
            && let Some(store) = control_store.as_deref_mut()
        {
            store.reset_to_frozen()?;
        }
        let evidence = match control_store.as_deref() {
            Some(store) => diagnostics_adapter::run_diagnostics_operation_on_store(
                store.store()?,
                store.path(),
                diagnostics_operation_kind(operation),
                seed,
                fault,
            )?,
            None => diagnostics_adapter::run_diagnostics_operation(
                diagnostics_operation_kind(operation),
                seed,
                fault,
            )?,
        };
        let (id, checker, input, observed, result) = match evidence.invariant {
            diagnostics_adapter::DiagnosticsInvariantEvidence::I63 { input, observed } => {
                let result = diagnostics_oracle::compare_i63(&input, &observed);
                (
                    63,
                    diagnostics_oracle::I63_CHECKER_ID,
                    input,
                    observed,
                    result,
                )
            }
            diagnostics_adapter::DiagnosticsInvariantEvidence::I64 { input, observed } => {
                let result = diagnostics_oracle::compare_i64(&input, &observed);
                (
                    64,
                    diagnostics_oracle::I64_CHECKER_ID,
                    input,
                    observed,
                    result,
                )
            }
            diagnostics_adapter::DiagnosticsInvariantEvidence::I65 { input, observed } => {
                let result = diagnostics_oracle::compare_i65(&input, &observed);
                (
                    65,
                    diagnostics_oracle::I65_CHECKER_ID,
                    input,
                    observed,
                    result,
                )
            }
        };
        push_feature_json_record(
            id,
            checker,
            operation.key(),
            diagnostics_input_json(&input),
            diagnostics_observed_json(&observed),
            format!("public diagnostics operation seed={seed}"),
            result,
            oracle_records,
            coverage,
        );
        let _ = control_records;
        receipts.extend(
            evidence
                .receipts
                .into_iter()
                .map(ProductionFeatureReceipt::ValidatedDiagnostics),
        );
    }
    Ok(receipts)
}

fn ffi_operation_kind(operation: super::campaign::FfiOperation) -> ffi_adapter::FfiOperationKind {
    match operation {
        super::campaign::FfiOperation::Validation => ffi_adapter::FfiOperationKind::Validation,
        super::campaign::FfiOperation::Ownership => ffi_adapter::FfiOperationKind::Ownership,
        super::campaign::FfiOperation::Containment => ffi_adapter::FfiOperationKind::Containment,
        super::campaign::FfiOperation::Deadline => ffi_adapter::FfiOperationKind::Deadline,
        super::campaign::FfiOperation::Parity => ffi_adapter::FfiOperationKind::Parity,
    }
}

fn ffi_fault_kind(
    fault: super::campaign::FeatureFault,
) -> Result<ffi_adapter::FfiFaultKind, String> {
    match fault {
        super::campaign::FeatureFault::FfiInvalidPointerShape => {
            Ok(ffi_adapter::FfiFaultKind::InvalidPointerShape)
        }
        super::campaign::FeatureFault::FfiInvalidEnum => Ok(ffi_adapter::FfiFaultKind::InvalidEnum),
        super::campaign::FeatureFault::FfiStaleHandle => Ok(ffi_adapter::FfiFaultKind::StaleHandle),
        super::campaign::FeatureFault::FfiDoubleDestroy => {
            Ok(ffi_adapter::FfiFaultKind::DoubleDestroy)
        }
        super::campaign::FeatureFault::FfiPanicBoundary => {
            Ok(ffi_adapter::FfiFaultKind::PanicBoundary)
        }
        super::campaign::FeatureFault::FfiMalformedSequence => {
            Ok(ffi_adapter::FfiFaultKind::MalformedSequence)
        }
        other => Err(format!("{} is not an FFI fault", other.key())),
    }
}

fn ffi_input_json(input: &ffi_oracle::FfiInput) -> String {
    format!(
        "{{\"expected_abi_version\":{}}}",
        input.expected_abi_version
    )
}

fn ffi_observed_json(observed: &ffi_oracle::FfiObserved) -> String {
    format!(
        "{{\"null_pointer_rejected\":{},\"invalid_enum_rejected\":{},\"stale_handle_rejected\":{},\"double_destroy_rejected\":{},\"panic_caught\":{},\"poisoned_after_panic\":{},\"control_cancelled_without_hits\":{},\"abi_version\":{},\"error_name_matches\":{},\"malformed_sequence_rejected\":{}}}",
        observed.null_pointer_rejected,
        observed.invalid_enum_rejected,
        observed.stale_handle_rejected,
        observed.double_destroy_rejected,
        observed.panic_caught,
        observed.poisoned_after_panic,
        observed.control_cancelled_without_hits,
        observed.abi_version,
        observed.error_name_matches,
        observed.malformed_sequence_rejected,
    )
}

fn run_ffi_campaign_operation(
    operation: super::campaign::FfiOperation,
    selected_faults: &[super::campaign::FeatureFault],
    seed: u64,
    oracle_records: &mut Vec<OracleRecord>,
    control_records: &mut Vec<String>,
    coverage: &mut CoverageRegistry,
    mut control_store: Option<&mut ControlStore>,
) -> Result<Vec<ProductionFeatureReceipt>, String> {
    let faults = selected_faults
        .iter()
        .copied()
        .map(ffi_fault_kind)
        .collect::<Result<Vec<_>, _>>()?;
    let cases = if faults.is_empty() {
        vec![None]
    } else {
        faults.into_iter().map(Some).collect()
    };
    let mut receipts = Vec::new();
    for (case_index, fault) in cases.into_iter().enumerate() {
        if case_index > 0
            && let Some(store) = control_store.as_deref_mut()
        {
            store.reset_to_frozen()?;
        }
        let case_identity = format!(
            "seed-{seed}-operation-{}-fault-{}",
            operation.key(),
            fault.map_or("none", ffi_adapter::FfiFaultKind::key)
        );
        let evidence = match control_store.as_deref_mut() {
            Some(store) => {
                store.store()?.stats().map_err(|error| error.to_string())?;
                let path = store.path().to_path_buf();
                store.close()?;
                ffi_adapter::run_ffi_operation_at_path(&path, ffi_operation_kind(operation), fault)?
            }
            None => ffi_adapter::run_ffi_operation(ffi_operation_kind(operation), fault)?,
        };
        let (id, checker, input, observed, result) = match evidence.invariant {
            ffi_adapter::FfiInvariantEvidence::I66 { input, observed } => {
                let result = ffi_oracle::compare_i66(&input, &observed);
                (66, ffi_oracle::I66_CHECKER_ID, input, observed, result)
            }
            ffi_adapter::FfiInvariantEvidence::I67 { input, observed } => {
                let result = ffi_oracle::compare_i67(&input, &observed);
                (67, ffi_oracle::I67_CHECKER_ID, input, observed, result)
            }
            ffi_adapter::FfiInvariantEvidence::I68 { input, observed } => {
                let result = ffi_oracle::compare_i68(&input, &observed);
                (68, ffi_oracle::I68_CHECKER_ID, input, observed, result)
            }
            ffi_adapter::FfiInvariantEvidence::I69 { input, observed } => {
                let result = ffi_oracle::compare_i69(&input, &observed);
                (69, ffi_oracle::I69_CHECKER_ID, input, observed, result)
            }
            ffi_adapter::FfiInvariantEvidence::I70 { input, observed } => {
                let result = ffi_oracle::compare_i70(&input, &observed);
                (70, ffi_oracle::I70_CHECKER_ID, input, observed, result)
            }
        };
        push_feature_json_record(
            id,
            checker,
            operation.key(),
            ffi_input_json(&input),
            ffi_observed_json(&observed),
            format!("public FFI operation seed={seed}"),
            result,
            oracle_records,
            coverage,
        );
        oracle_records
            .last_mut()
            .expect("FFI comparison appended one oracle record")
            .case_identity = Some(case_identity);
        let _ = control_records;
        receipts.extend(
            evidence
                .receipts
                .into_iter()
                .map(ProductionFeatureReceipt::ValidatedFfi),
        );
    }
    Ok(receipts)
}

fn hybrid_operation_kind(
    operation: super::campaign::HybridOperation,
) -> hybrid_adapter::HybridOperationKind {
    match operation {
        super::campaign::HybridOperation::Provenance => {
            hybrid_adapter::HybridOperationKind::Provenance
        }
        super::campaign::HybridOperation::Normalization => {
            hybrid_adapter::HybridOperationKind::Normalization
        }
        super::campaign::HybridOperation::BoundedFusion => {
            hybrid_adapter::HybridOperationKind::BoundedFusion
        }
        super::campaign::HybridOperation::Rrf => hybrid_adapter::HybridOperationKind::Rrf,
        super::campaign::HybridOperation::Legs => hybrid_adapter::HybridOperationKind::Legs,
    }
}

fn hybrid_fault_kind(
    fault: super::campaign::FeatureFault,
) -> Result<hybrid_adapter::HybridFaultKind, String> {
    match fault {
        super::campaign::FeatureFault::HybridVectorLegError => {
            Ok(hybrid_adapter::HybridFaultKind::VectorLegError)
        }
        super::campaign::FeatureFault::HybridLexicalLegError => {
            Ok(hybrid_adapter::HybridFaultKind::LexicalLegError)
        }
        super::campaign::FeatureFault::HybridDualFailureOrder => {
            Ok(hybrid_adapter::HybridFaultKind::DualFailureOrder)
        }
        super::campaign::FeatureFault::HybridLegPanic => {
            Ok(hybrid_adapter::HybridFaultKind::LegPanic)
        }
        super::campaign::FeatureFault::HybridEstimatedScore => {
            Ok(hybrid_adapter::HybridFaultKind::EstimatedScore)
        }
        super::campaign::FeatureFault::HybridNonfiniteScore => {
            Ok(hybrid_adapter::HybridFaultKind::NonfiniteScore)
        }
        super::campaign::FeatureFault::HybridCancelClose => {
            Ok(hybrid_adapter::HybridFaultKind::CancelClose)
        }
        other => Err(format!("{} is not a hybrid fault", other.key())),
    }
}

fn hybrid_input_json(input: &hybrid_oracle::HybridInput) -> String {
    format!(
        "{{\"generated_docs\":{},\"deleted_docs\":{},\"active_docs\":{},\"dimensions\":{},\"k\":{},\"alpha_bits\":{},\"vector_candidates\":{},\"lexical_candidates\":{},\"rrf_lexical_candidates\":{},\"single_lexical_candidates\":{},\"empty_lexical_candidates\":{}}}",
        input.generated_docs,
        input.deleted_docs,
        input.active_docs,
        input.dimensions,
        input.main.k,
        input.main.alpha_bits,
        input.main.vector.len(),
        input.main.lexical.len(),
        input.rrf.lexical.len(),
        input.single.lexical.len(),
        input.empty.lexical.len(),
    )
}

fn hybrid_observed_json(observed: &hybrid_oracle::HybridObserved) -> String {
    let hits = |values: &[hybrid_oracle::FusedHitFact]| {
        values
            .iter()
            .map(|hit| {
                let optional = |value: Option<u64>| {
                    value.map_or_else(|| "null".to_owned(), |bits| bits.to_string())
                };
                format!(
                    "{{\"id\":{},\"vector_bits\":{},\"lexical_bits\":{},\"fused_bits\":{}}}",
                    hit.id,
                    optional(hit.vector_squared_l2_bits),
                    optional(hit.lexical_bm25_bits),
                    hit.fused_score_bits,
                )
            })
            .collect::<Vec<_>>()
            .join(",")
    };
    let method = |value: hybrid_oracle::FusionMethodFact| match value {
        hybrid_oracle::FusionMethodFact::ConvexCombination => "convex-combination",
        hybrid_oracle::FusionMethodFact::ReciprocalRankFusion => "rrf",
    };
    format!(
        "{{\"main_hits\":[{}],\"main_method\":\"{}\",\"main_rounds\":{},\"rrf_hits\":[{}],\"rrf_method\":\"{}\",\"single_method\":\"{}\",\"empty_method\":\"{}\",\"leg_faults\":{},\"same_seed_control_passed\":{}}}",
        hits(&observed.main.hits),
        method(observed.main.report.method),
        observed.main.report.rounds,
        hits(&observed.rrf.hits),
        method(observed.rrf.report.method),
        method(observed.single.report.method),
        method(observed.empty.report.method),
        observed.leg_faults.len(),
        observed.same_seed_control_passed,
    )
}

fn run_hybrid_campaign_operation(
    operation: super::campaign::HybridOperation,
    selected_faults: &[super::campaign::FeatureFault],
    episode: &hybrid_adapter::HybridEpisode,
    seed: u64,
    oracle_records: &mut Vec<OracleRecord>,
    control_records: &mut Vec<String>,
    coverage: &mut CoverageRegistry,
) -> Result<Vec<ProductionFeatureReceipt>, String> {
    let faults = selected_faults
        .iter()
        .copied()
        .map(hybrid_fault_kind)
        .collect::<Result<Vec<_>, _>>()?;
    let cases = if faults.is_empty() {
        vec![None]
    } else {
        faults.into_iter().map(Some).collect()
    };
    let mut receipts = Vec::new();
    for fault in cases {
        let case_identity = format!(
            "seed-{seed}-operation-{}-fault-{}",
            operation.key(),
            fault.map_or("none", hybrid_adapter::HybridFaultKind::key)
        );
        let evidence =
            hybrid_adapter::run_hybrid_operation(episode, hybrid_operation_kind(operation), fault)?;
        for invariant in evidence.invariants {
            let (id, checker, input, observed, result) = match invariant {
                hybrid_adapter::HybridInvariantEvidence::I45 { input, observed } => {
                    let result = hybrid_oracle::compare_i45(&input, &observed);
                    (45, hybrid_oracle::I45_CHECKER_ID, input, observed, result)
                }
                hybrid_adapter::HybridInvariantEvidence::I46 { input, observed } => {
                    let result = hybrid_oracle::compare_i46(&input, &observed);
                    (46, hybrid_oracle::I46_CHECKER_ID, input, observed, result)
                }
                hybrid_adapter::HybridInvariantEvidence::I47 { input, observed } => {
                    let result = hybrid_oracle::compare_i47(&input, &observed);
                    (47, hybrid_oracle::I47_CHECKER_ID, input, observed, result)
                }
                hybrid_adapter::HybridInvariantEvidence::I48 { input, observed } => {
                    let result = hybrid_oracle::compare_i48(&input, &observed);
                    (48, hybrid_oracle::I48_CHECKER_ID, input, observed, result)
                }
                hybrid_adapter::HybridInvariantEvidence::I49 { input, observed } => {
                    let result = hybrid_oracle::compare_i49(&input, &observed);
                    (49, hybrid_oracle::I49_CHECKER_ID, input, observed, result)
                }
            };
            push_feature_json_record(
                id,
                checker,
                operation.key(),
                hybrid_input_json(&input),
                hybrid_observed_json(&observed),
                format!("public hybrid operation seed={seed}"),
                result,
                oracle_records,
                coverage,
            );
            oracle_records
                .last_mut()
                .expect("hybrid comparison appended one oracle record")
                .case_identity = Some(case_identity.clone());
        }
        if fault.is_some() {
            control_records.push(format!(
                "{{\"campaign\":\"hybrid-fusion\",\"operation\":\"{}\",\"seed\":{seed},\"clean_control_passed\":{}}}",
                operation.key(), evidence.clean_control_passed
            ));
        }
        receipts.extend(
            evidence
                .receipts
                .into_iter()
                .map(ProductionFeatureReceipt::ValidatedHybrid),
        );
    }
    Ok(receipts)
}

fn fts_operation_kind(operation: super::campaign::FtsOperation) -> fts_adapter::FtsOperationKind {
    match operation {
        super::campaign::FtsOperation::Tokenizer => fts_adapter::FtsOperationKind::Tokenizer,
        super::campaign::FtsOperation::Regions => fts_adapter::FtsOperationKind::Regions,
        super::campaign::FtsOperation::Bm25 => fts_adapter::FtsOperationKind::Bm25,
        super::campaign::FtsOperation::Pruning => fts_adapter::FtsOperationKind::Pruning,
        super::campaign::FtsOperation::Extras => fts_adapter::FtsOperationKind::Extras,
    }
}

fn fts_fault_kind(
    fault: super::campaign::FeatureFault,
) -> Result<fts_adapter::FtsFaultKind, String> {
    match fault {
        super::campaign::FeatureFault::FtsPostingsCorruption => {
            Ok(fts_adapter::FtsFaultKind::PostingsCorruption)
        }
        super::campaign::FeatureFault::FtsDictionaryCorruption => {
            Ok(fts_adapter::FtsFaultKind::DictionaryCorruption)
        }
        super::campaign::FeatureFault::FtsNormCorruption => {
            Ok(fts_adapter::FtsFaultKind::NormCorruption)
        }
        super::campaign::FeatureFault::FtsBlockMaxCorruption => {
            Ok(fts_adapter::FtsFaultKind::BlockMaxCorruption)
        }
        super::campaign::FeatureFault::FtsStoredTextCorruption => {
            Ok(fts_adapter::FtsFaultKind::StoredTextCorruption)
        }
        super::campaign::FeatureFault::FtsStoredTextAbsence => {
            Ok(fts_adapter::FtsFaultKind::StoredTextAbsence)
        }
        super::campaign::FeatureFault::FtsLexicalCancellation => {
            Ok(fts_adapter::FtsFaultKind::LexicalCancellation)
        }
        other => Err(format!("{} is not an FTS fault", other.key())),
    }
}

fn fts_input_json(input: &fts_oracle::FtsInput) -> String {
    let terms = input
        .bm25_terms
        .iter()
        .map(|term| format!("\"{}\"", json_escape(term)))
        .collect::<Vec<_>>()
        .join(",");
    let sealed = input
        .documents
        .iter()
        .filter(|document| document.sealed)
        .count();
    let active = input.documents.len().saturating_sub(sealed);
    let deleted = input
        .documents
        .iter()
        .filter(|document| document.deleted)
        .count();
    format!(
        "{{\"documents\":{},\"sealed\":{sealed},\"active\":{active},\"deleted\":{deleted},\"bm25_terms\":[{terms}],\"bm25_k\":{}}}",
        input.documents.len(),
        input.bm25_k,
    )
}

fn fts_observed_json(observed: &fts_oracle::FtsObserved) -> String {
    let hits = |values: &[fts_oracle::ScoreFact]| {
        values
            .iter()
            .map(|hit| {
                format!(
                    "{{\"doc_id\":{},\"score_bits\":{}}}",
                    hit.doc_id, hit.score_bits
                )
            })
            .collect::<Vec<_>>()
            .join(",")
    };
    let token_count = observed
        .tokens
        .iter()
        .map(|document| document.tokens.len())
        .sum::<usize>();
    format!(
        "{{\"token_documents\":{},\"token_count\":{token_count},\"sealed_segment_bytes\":{},\"bm25_hits\":[{}],\"wand_hits\":[{}],\"maxscore_hits\":[{}],\"wand_blocks_skipped\":{},\"maxscore_blocks_skipped\":{},\"phrase_results\":{},\"prefix_results\":{},\"fuzzy_results\":{},\"phonetic_results\":{}}}",
        observed.tokens.len(),
        observed.sealed_segment.len(),
        hits(&observed.bm25_hits),
        hits(&observed.wand_hits),
        hits(&observed.maxscore_hits),
        observed.wand_blocks_skipped,
        observed.maxscore_blocks_skipped,
        observed.phrase.result_ids.len(),
        observed.prefix.result_ids.len(),
        observed.fuzzy.result_ids.len(),
        observed.phonetic.result_ids.len(),
    )
}

fn run_fts_campaign_operation(
    operation: super::campaign::FtsOperation,
    selected_faults: &[super::campaign::FeatureFault],
    seed: u64,
    oracle_records: &mut Vec<OracleRecord>,
    control_records: &mut Vec<String>,
    coverage: &mut CoverageRegistry,
    mut control_store: Option<&mut ControlStore>,
) -> Result<Vec<ProductionFeatureReceipt>, String> {
    let faults = selected_faults
        .iter()
        .copied()
        .map(fts_fault_kind)
        .collect::<Result<Vec<_>, _>>()?;
    let cases = if faults.is_empty() {
        vec![None]
    } else {
        faults.into_iter().map(Some).collect()
    };
    let mut receipts = Vec::new();
    for (case_index, fault) in cases.into_iter().enumerate() {
        if case_index > 0
            && let Some(store) = control_store.as_deref_mut()
        {
            store.reset_to_frozen()?;
        }
        let case_identity = format!(
            "seed-{seed}-operation-{}-fault-{}",
            operation.key(),
            fault.map_or("none", fts_adapter::FtsFaultKind::key)
        );
        let evidence = match control_store.as_deref_mut() {
            Some(store) => fts_adapter::run_fts_operation_on_store(
                store,
                fts_operation_kind(operation),
                seed,
                fault,
            )?,
            None => fts_adapter::run_fts_operation(fts_operation_kind(operation), seed, fault)?,
        };
        for invariant in evidence.invariants {
            let (id, checker, input, observed, result) = match invariant {
                fts_adapter::FtsInvariantEvidence::I40 { input, observed } => {
                    let result = fts_oracle::compare_i40(&input, &observed);
                    (40, fts_oracle::I40_CHECKER_ID, input, observed, result)
                }
                fts_adapter::FtsInvariantEvidence::I41 { input, observed } => {
                    let result = fts_oracle::compare_i41(&input, &observed);
                    (41, fts_oracle::I41_CHECKER_ID, input, observed, result)
                }
                fts_adapter::FtsInvariantEvidence::I42 { input, observed } => {
                    let result = fts_oracle::compare_i42(&input, &observed);
                    (42, fts_oracle::I42_CHECKER_ID, input, observed, result)
                }
                fts_adapter::FtsInvariantEvidence::I43 { input, observed } => {
                    let result = fts_oracle::compare_i43(&input, &observed);
                    (43, fts_oracle::I43_CHECKER_ID, input, observed, result)
                }
                fts_adapter::FtsInvariantEvidence::I44 { input, observed } => {
                    let result = fts_oracle::compare_i44(&input, &observed);
                    (44, fts_oracle::I44_CHECKER_ID, input, observed, result)
                }
            };
            push_feature_json_record(
                id,
                checker,
                operation.key(),
                fts_input_json(&input),
                fts_observed_json(&observed),
                format!("public FTS operation seed={seed}"),
                result,
                oracle_records,
                coverage,
            );
            oracle_records
                .last_mut()
                .expect("FTS comparison appended one oracle record")
                .case_identity = Some(case_identity.clone());
        }
        let _ = control_records;
        receipts.extend(
            evidence
                .receipts
                .into_iter()
                .map(ProductionFeatureReceipt::ValidatedFts),
        );
    }
    Ok(receipts)
}

fn graph_operation_kind(
    operation: super::campaign::GraphOperation,
) -> graph_adapter::GraphOperationKind {
    match operation {
        super::campaign::GraphOperation::Shape => graph_adapter::GraphOperationKind::Shape,
        super::campaign::GraphOperation::EntryPoints => {
            graph_adapter::GraphOperationKind::EntryPoints
        }
        super::campaign::GraphOperation::Checkpoint => {
            graph_adapter::GraphOperationKind::Checkpoint
        }
        super::campaign::GraphOperation::BoundedBuild => {
            graph_adapter::GraphOperationKind::BoundedBuild
        }
        super::campaign::GraphOperation::Search => graph_adapter::GraphOperationKind::Search,
        super::campaign::GraphOperation::Publication => {
            graph_adapter::GraphOperationKind::Publication
        }
        super::campaign::GraphOperation::FilteredSearch => {
            graph_adapter::GraphOperationKind::FilteredSearch
        }
    }
}

fn graph_fault_kind(
    fault: super::campaign::FeatureFault,
) -> Result<graph_adapter::GraphFaultKind, String> {
    match fault {
        super::campaign::FeatureFault::GraphCheckpointCorruption => {
            Ok(graph_adapter::GraphFaultKind::CheckpointCorruption)
        }
        super::campaign::FeatureFault::GraphBuildBudgetCancel => {
            Ok(graph_adapter::GraphFaultKind::BuildBudgetCancel)
        }
        super::campaign::FeatureFault::GraphCorruptNode => {
            Ok(graph_adapter::GraphFaultKind::CorruptNode)
        }
        super::campaign::FeatureFault::GraphCorruptEntry => {
            Ok(graph_adapter::GraphFaultKind::CorruptEntry)
        }
        super::campaign::FeatureFault::GraphMissingRescore => {
            Ok(graph_adapter::GraphFaultKind::MissingRescore)
        }
        super::campaign::FeatureFault::GraphSearchCancellation => {
            Ok(graph_adapter::GraphFaultKind::SearchCancellation)
        }
        super::campaign::FeatureFault::GraphPublicationCrash => {
            Ok(graph_adapter::GraphFaultKind::PublicationCrash)
        }
        other => Err(format!("{} is not a graph fault", other.key())),
    }
}

fn graph_input_json(input: &graph_oracle::GraphInput) -> String {
    let documents = input
        .rows
        .iter()
        .map(|row| row.document.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let deleted = input
        .rows
        .iter()
        .filter(|row| row.deleted)
        .map(|row| row.document.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let query = input
        .query_bits
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let live = input.rows.iter().filter(|row| !row.deleted).count();
    format!(
        "{{\"seed\":{},\"dims\":{},\"k\":{},\"row_count\":{},\"live_row_count\":{live},\"documents\":[{documents}],\"deleted_documents\":[{deleted}],\"query_bits\":[{query}],\"fixture_digest\":\"fnv1a64:{:016x}\"}}",
        input.seed,
        input.dims,
        input.k,
        input.rows.len(),
        graph_oracle::fixture_digest(input),
    )
}

fn graph_observed_json(observed: &graph_oracle::GraphObserved) -> String {
    let id = |value: &[u8; 16]| {
        value
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    };
    let ids = |values: &[[u8; 16]]| {
        values
            .iter()
            .map(|value| format!("\"{}\"", id(value)))
            .collect::<Vec<_>>()
            .join(",")
    };
    let rows = |values: &[u32]| {
        values
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",")
    };
    let documents = |values: &[u64]| {
        values
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(",")
    };
    let candidates = |values: &[graph_oracle::GraphCandidate]| {
        values
            .iter()
            .map(|candidate| {
                format!(
                    "{{\"document\":{},\"score_bits\":{},\"segment\":\"{}\",\"row\":{}}}",
                    candidate.document,
                    candidate.score_bits,
                    id(&candidate.segment),
                    candidate.row,
                )
            })
            .collect::<Vec<_>>()
            .join(",")
    };
    format!(
        "{{\"graph_node_count\":{},\"production_live_rows\":{},\"graph_max_degree\":{},\"maximum_observed_degree\":{},\"entry_rows\":[{}],\"entry_documents\":[{}],\"graphs_built\":{},\"graph_segments\":{},\"entry_discoveries\":{},\"exact_rescore\":{},\"graph\":[{}],\"exact\":[{}],\"exact_all\":[{}],\"bounded_budget_exhausted\":{},\"checkpoint_exists_after_bounded\":{},\"bounded_bytes_consumed\":{},\"work_stride\":{},\"checkpoints_resumed\":{},\"checkpoint_removed_after_resume\":{},\"manifest_segments\":[{}],\"graph_segments_on_disk\":{},\"source_segment\":\"{}\",\"source_file_exists\":{},\"source_manifest_referenced\":{},\"temporary_orphans\":{},\"reopened_graph\":[{}],\"filtered_graph\":[{}],\"filtered_graph_exact\":[{}],\"filtered_large_cardinality\":{},\"filtered_graph_branch\":{},\"filtered_graph_exact_rescore\":{},\"filtered_small\":[{}],\"filtered_small_exact\":[{}],\"filtered_small_cardinality\":{},\"filtered_small_exact_allow_list\":{}}}",
        observed.graph_node_count,
        observed.production_live_rows,
        observed.graph_max_degree,
        observed.maximum_observed_degree,
        rows(&observed.entry_rows),
        documents(&observed.entry_documents),
        observed.graphs_built,
        observed.graph_segments,
        observed.entry_discoveries,
        observed.exact_rescore,
        candidates(&observed.graph),
        candidates(&observed.exact),
        candidates(&observed.exact_all),
        observed.bounded_budget_exhausted,
        observed.checkpoint_exists_after_bounded,
        observed.bounded_bytes_consumed,
        observed.work_stride,
        observed.checkpoints_resumed,
        observed.checkpoint_removed_after_resume,
        ids(&observed.manifest_segments),
        observed.graph_segments_on_disk,
        id(&observed.source_segment),
        observed.source_file_exists,
        observed.source_manifest_referenced,
        observed.temporary_orphans,
        candidates(&observed.reopened_graph),
        candidates(&observed.filtered_graph),
        candidates(&observed.filtered_graph_exact),
        observed.filtered_large_cardinality,
        observed.filtered_graph_branch,
        observed.filtered_graph_exact_rescore,
        candidates(&observed.filtered_small),
        candidates(&observed.filtered_small_exact),
        observed.filtered_small_cardinality,
        observed.filtered_small_exact_allow_list,
    )
}

fn run_graph_campaign_operation(
    operation: super::campaign::GraphOperation,
    selected_faults: &[super::campaign::FeatureFault],
    seed: u64,
    oracle_records: &mut Vec<OracleRecord>,
    control_records: &mut Vec<String>,
    coverage: &mut CoverageRegistry,
    mut control_store: Option<&mut ControlStore>,
) -> Result<Vec<ProductionFeatureReceipt>, String> {
    let faults = selected_faults
        .iter()
        .copied()
        .map(graph_fault_kind)
        .collect::<Result<Vec<_>, _>>()?;
    let cases = if faults.is_empty() {
        vec![None]
    } else {
        faults.into_iter().map(Some).collect()
    };
    let mut receipts = Vec::new();
    for (case_index, fault) in cases.into_iter().enumerate() {
        if case_index > 0
            && let Some(store) = control_store.as_deref_mut()
        {
            store.reset_to_frozen()?;
        }
        let case_identity = format!(
            "seed-{seed}-operation-{}-fault-{}",
            operation.key(),
            fault.map_or("none", graph_adapter::GraphFaultKind::key)
        );
        let evidence = match control_store.as_deref_mut() {
            Some(store) => graph_adapter::run_graph_operation_on_store(
                store,
                graph_operation_kind(operation),
                seed,
                fault,
            )?,
            None => {
                graph_adapter::run_graph_operation(graph_operation_kind(operation), seed, fault)?
            }
        };
        for invariant in evidence.invariants {
            let (id, checker, input, observed, result) = match invariant {
                graph_adapter::GraphInvariantEvidence::I28 { input, observed } => {
                    let result = graph_oracle::compare_i28(&input, &observed);
                    (28, graph_oracle::I28_CHECKER_ID, input, observed, result)
                }
                graph_adapter::GraphInvariantEvidence::I29 { input, observed } => {
                    let result = graph_oracle::compare_i29(&input, &observed);
                    (29, graph_oracle::I29_CHECKER_ID, input, observed, result)
                }
                graph_adapter::GraphInvariantEvidence::I30 { input, observed } => {
                    let result = graph_oracle::compare_i30(&input, &observed);
                    (30, graph_oracle::I30_CHECKER_ID, input, observed, result)
                }
                graph_adapter::GraphInvariantEvidence::I31 { input, observed } => {
                    let result = graph_oracle::compare_i31(&input, &observed);
                    (31, graph_oracle::I31_CHECKER_ID, input, observed, result)
                }
                graph_adapter::GraphInvariantEvidence::I32 { input, observed } => {
                    let result = graph_oracle::compare_i32(&input, &observed);
                    (32, graph_oracle::I32_CHECKER_ID, input, observed, result)
                }
                graph_adapter::GraphInvariantEvidence::I33 { input, observed } => {
                    let result = graph_oracle::compare_i33(&input, &observed);
                    (33, graph_oracle::I33_CHECKER_ID, input, observed, result)
                }
                graph_adapter::GraphInvariantEvidence::I34 { input, observed } => {
                    let result = graph_oracle::compare_i34(&input, &observed);
                    (34, graph_oracle::I34_CHECKER_ID, input, observed, result)
                }
                graph_adapter::GraphInvariantEvidence::I35 { input, observed } => {
                    let result = graph_oracle::compare_i35(&input, &observed);
                    (35, graph_oracle::I35_CHECKER_ID, input, observed, result)
                }
            };
            push_feature_json_record(
                id,
                checker,
                operation.key(),
                graph_input_json(&input),
                graph_observed_json(&observed),
                format!("public graph operation seed={seed}"),
                result,
                oracle_records,
                coverage,
            );
            oracle_records
                .last_mut()
                .expect("graph comparison appended one oracle record")
                .case_identity = Some(case_identity.clone());
        }
        let _ = control_records;
        receipts.extend(
            evidence
                .receipts
                .into_iter()
                .map(ProductionFeatureReceipt::ValidatedGraph),
        );
    }
    match operation {
        super::campaign::GraphOperation::Search => coverage.hit("search.graph"),
        super::campaign::GraphOperation::FilteredSearch => coverage.hit("search.filtered_graph"),
        _ => {}
    }
    Ok(receipts)
}

#[cfg(test)]
mod graph_campaign_tests {
    use super::*;

    #[test]
    fn graph_campaign_dispatch_records_every_invariant_and_fault_receipt() {
        let operations = [
            super::super::campaign::GraphOperation::Shape,
            super::super::campaign::GraphOperation::EntryPoints,
            super::super::campaign::GraphOperation::Checkpoint,
            super::super::campaign::GraphOperation::BoundedBuild,
            super::super::campaign::GraphOperation::Search,
            super::super::campaign::GraphOperation::Publication,
            super::super::campaign::GraphOperation::FilteredSearch,
        ];
        let mut records = Vec::new();
        let mut controls = Vec::new();
        let mut coverage = CoverageRegistry::default();
        for operation in operations {
            let receipts = run_graph_campaign_operation(
                operation,
                &[],
                9,
                &mut records,
                &mut controls,
                &mut coverage,
                None,
            )
            .expect("clean graph campaign operation");
            assert!(receipts.is_empty());
        }
        assert_eq!(records.len(), 8);
        assert!(records.iter().all(|record| record.passed));
        for invariant in 28..=35 {
            assert_eq!(
                coverage.count(&format!("invariant.I{invariant}.checked")),
                1
            );
        }

        for fault in [
            super::super::campaign::FeatureFault::GraphCheckpointCorruption,
            super::super::campaign::FeatureFault::GraphBuildBudgetCancel,
            super::super::campaign::FeatureFault::GraphCorruptNode,
            super::super::campaign::FeatureFault::GraphCorruptEntry,
            super::super::campaign::FeatureFault::GraphMissingRescore,
            super::super::campaign::FeatureFault::GraphSearchCancellation,
            super::super::campaign::FeatureFault::GraphPublicationCrash,
        ] {
            let operation = match fault.operation() {
                super::super::campaign::FeatureOperation::Graph(operation) => operation,
                _ => panic!("graph fault escaped graph operation"),
            };
            let receipts = run_graph_campaign_operation(
                operation,
                &[fault],
                10,
                &mut records,
                &mut controls,
                &mut coverage,
                None,
            )
            .unwrap_or_else(|error| panic!("{}: {error}", fault.key()));
            assert_eq!(receipts.len(), 1);
            assert_eq!(receipts[0].fault(), Some(fault.key()));
            assert_eq!(receipts[0].cardinality(), Some(1));
        }

        let start = records.len();
        run_graph_campaign_operation(
            super::super::campaign::GraphOperation::Search,
            &[
                super::super::campaign::FeatureFault::GraphMissingRescore,
                super::super::campaign::FeatureFault::GraphSearchCancellation,
            ],
            83,
            &mut records,
            &mut controls,
            &mut coverage,
            None,
        )
        .expect("two graph search faults at one operation");
        let identities = records[start..]
            .iter()
            .map(|record| {
                (
                    record.checker_id,
                    record
                        .case_identity
                        .as_deref()
                        .expect("graph case identity"),
                )
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(identities.len(), records.len() - start);
    }
}

fn ingest_operation_kind(
    operation: super::campaign::IngestOperation,
) -> ingest_adapter::IngestOperationKind {
    match operation {
        super::campaign::IngestOperation::BatchCommit => {
            ingest_adapter::IngestOperationKind::BatchCommit
        }
        super::campaign::IngestOperation::Seal => ingest_adapter::IngestOperationKind::Seal,
        super::campaign::IngestOperation::Retention => {
            ingest_adapter::IngestOperationKind::Retention
        }
        super::campaign::IngestOperation::Purge => ingest_adapter::IngestOperationKind::Purge,
    }
}

fn ingest_fault_kind(
    fault: super::campaign::FeatureFault,
) -> Result<ingest_adapter::IngestFaultKind, String> {
    match fault {
        super::campaign::FeatureFault::IngestPostAckRetry => {
            Ok(ingest_adapter::IngestFaultKind::PostAckRetry)
        }
        super::campaign::FeatureFault::IngestPartialBatchAppend => {
            Ok(ingest_adapter::IngestFaultKind::PartialBatchAppend)
        }
        super::campaign::FeatureFault::IngestSealCancellation => {
            Ok(ingest_adapter::IngestFaultKind::SealCancellation)
        }
        super::campaign::FeatureFault::IngestRetentionClockBoundary => {
            Ok(ingest_adapter::IngestFaultKind::RetentionClockBoundary)
        }
        super::campaign::FeatureFault::IngestPurgeUnlinkError => {
            Ok(ingest_adapter::IngestFaultKind::PurgeUnlinkError)
        }
        super::campaign::FeatureFault::IngestPurgeCrashBoundary => {
            Ok(ingest_adapter::IngestFaultKind::PurgeCrashBoundary)
        }
        other => Err(format!(
            "feature fault {} is not an ingest-retention fault",
            other.key()
        )),
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "family dispatch keeps the shared evidence sinks explicit"
)]
fn run_ingest_campaign_operation(
    operation: super::campaign::IngestOperation,
    selected_faults: &[super::campaign::FeatureFault],
    seed: u64,
    profile: FaultProfile,
    op_index: usize,
    oracle_records: &mut Vec<OracleRecord>,
    control_records: &mut Vec<String>,
    family_artifact_records: &mut BTreeMap<&'static str, Vec<String>>,
    coverage: &mut CoverageRegistry,
) -> Result<Vec<ProductionFeatureReceipt>, String> {
    let matching_faults = selected_faults
        .iter()
        .copied()
        .filter(|fault| fault.operation() == super::campaign::FeatureOperation::Ingest(operation))
        .map(ingest_fault_kind)
        .map(|fault| fault.map(Some))
        .collect::<Result<Vec<_>, _>>()?;
    let cases = if matching_faults.is_empty() {
        vec![None]
    } else {
        matching_faults
    };
    let mut receipts = Vec::new();
    for fault in cases {
        let invocation_id = u64::try_from(op_index)
            .map_err(|_| "ingest-retention operation index exceeds u64".to_owned())?;
        let evidence = ingest_adapter::run_ingest_operation_with_invocation(
            ingest_operation_kind(operation),
            seed,
            fault,
            invocation_id,
        )?;
        record_ingest_family_artifacts(
            operation,
            fault,
            invocation_id,
            &evidence,
            family_artifact_records,
        )?;
        match evidence {
            ingest_adapter::IngestOperationEvidence::I20(evidence) => {
                push_ingest_feature_record(
                    20,
                    ingest_oracle::I20_CHECKER_ID,
                    operation.key(),
                    ingest_adapter::ingest_case_identity(
                        ingest_operation_kind(operation),
                        fault,
                        invocation_id,
                    ),
                    &evidence.expected,
                    &evidence.observed,
                    format!(
                        "public Store ingest/seal/search/reopen seed={seed} profile={}",
                        profile.key()
                    ),
                    ingest_oracle::attest_i20(&evidence.expected, &evidence.observed),
                    ingest_oracle::compare_i20(&evidence.expected, &evidence.observed),
                    oracle_records,
                    coverage,
                );
                match fault {
                    None => {
                        if evidence.control.is_some() || !evidence.receipts.is_empty() {
                            return Err(
                                "clean I20 evidence carried fault control or receipts".to_owned()
                            );
                        }
                    }
                    Some(fault) => {
                        let control = evidence.control.as_ref().ok_or_else(|| {
                            format!("I20 fault {fault:?} omitted its same-seed control")
                        })?;
                        if !control.isolated_directories
                            || !control.passed
                            || control.clean_initial_directory != control.fault_initial_directory
                            || control.clean_final != control.fault_final
                        {
                            return Err(format!(
                                "I20 fault {fault:?} failed its byte-identical same-seed relation"
                            ));
                        }
                        let [receipt] = evidence.receipts.as_slice() else {
                            return Err(format!(
                                "I20 fault {fault:?} produced {} typed Store receipts, expected 1",
                                evidence.receipts.len()
                            ));
                        };
                        validate_ingest_receipt(fault, operation, invocation_id, receipt)?;
                        control_records.push(ingest_control_json(seed, operation, fault, control));
                        receipts.push(ProductionFeatureReceipt::ValidatedIngestRetention(
                            receipt.clone(),
                        ));
                    }
                }
            }
            ingest_adapter::IngestOperationEvidence::I21(evidence) => {
                push_ingest_feature_record(
                    21,
                    ingest_oracle::I21_CHECKER_ID,
                    operation.key(),
                    ingest_adapter::ingest_case_identity(
                        ingest_operation_kind(operation),
                        fault,
                        invocation_id,
                    ),
                    &evidence.expected,
                    &evidence.observed,
                    format!(
                        "public Store ingest/delete/seal/search/reopen seed={seed} profile={}",
                        profile.key()
                    ),
                    ingest_oracle::attest_i21(&evidence.expected, &evidence.observed),
                    ingest_oracle::compare_i21(&evidence.expected, &evidence.observed),
                    oracle_records,
                    coverage,
                );
                match fault {
                    None => {
                        if evidence.control.is_some() || !evidence.receipts.is_empty() {
                            return Err(
                                "clean I21 evidence carried fault control or receipts".to_owned()
                            );
                        }
                    }
                    Some(fault) => {
                        let control = evidence.control.as_ref().ok_or_else(|| {
                            format!("I21 fault {fault:?} omitted its same-seed control")
                        })?;
                        if !control.isolated_directories
                            || !control.passed
                            || control.clean_initial_directory != control.fault_initial_directory
                            || control.clean_final != control.fault_final
                        {
                            return Err(format!(
                                "I21 fault {fault:?} failed its byte-identical same-seed relation"
                            ));
                        }
                        let [receipt] = evidence.receipts.as_slice() else {
                            return Err(format!(
                                "I21 fault {fault:?} produced {} typed Store receipts, expected 1",
                                evidence.receipts.len()
                            ));
                        };
                        validate_ingest_receipt(fault, operation, invocation_id, receipt)?;
                        control_records
                            .push(ingest_i21_control_json(seed, operation, fault, control));
                        receipts.push(ProductionFeatureReceipt::ValidatedIngestRetention(
                            receipt.clone(),
                        ));
                    }
                }
            }
            ingest_adapter::IngestOperationEvidence::I22(evidence) => {
                push_ingest_feature_record(
                    22,
                    ingest_oracle::I22_CHECKER_ID,
                    operation.key(),
                    ingest_adapter::ingest_case_identity(
                        ingest_operation_kind(operation),
                        fault,
                        invocation_id,
                    ),
                    &evidence.expected,
                    &evidence.observed,
                    format!(
                        "public Store ingest/seal/apply-retention/search/reopen seed={seed} profile={}",
                        profile.key()
                    ),
                    ingest_oracle::attest_i22(&evidence.expected, &evidence.observed),
                    ingest_oracle::compare_i22(&evidence.expected, &evidence.observed),
                    oracle_records,
                    coverage,
                );
                match fault {
                    None => {
                        if evidence.control.is_some() || !evidence.receipts.is_empty() {
                            return Err(
                                "clean I22 evidence carried fault control or receipts".to_owned()
                            );
                        }
                    }
                    Some(fault) => {
                        let control = evidence.control.as_ref().ok_or_else(|| {
                            format!("I22 fault {fault:?} omitted its same-seed control")
                        })?;
                        if !control.isolated_directories
                            || !control.passed
                            || control.clean_initial_directory != control.fault_initial_directory
                            || control.clean_final != control.fault_final
                        {
                            return Err(format!(
                                "I22 fault {fault:?} failed its byte-identical same-seed relation"
                            ));
                        }
                        let [receipt] = evidence.receipts.as_slice() else {
                            return Err(format!(
                                "I22 fault {fault:?} produced {} typed Store receipts, expected 1",
                                evidence.receipts.len()
                            ));
                        };
                        validate_ingest_receipt(fault, operation, invocation_id, receipt)?;
                        control_records
                            .push(ingest_i22_control_json(seed, operation, fault, control));
                        receipts.push(ProductionFeatureReceipt::ValidatedIngestRetention(
                            receipt.clone(),
                        ));
                    }
                }
            }
            ingest_adapter::IngestOperationEvidence::I23(evidence) => {
                push_ingest_feature_record(
                    23,
                    ingest_oracle::I23_CHECKER_ID,
                    operation.key(),
                    ingest_adapter::ingest_case_identity(
                        ingest_operation_kind(operation),
                        fault,
                        invocation_id,
                    ),
                    &evidence.expected,
                    &evidence.observed,
                    format!(
                        "public Store delete/purge/await/byte-scan/search/two-reopens seed={seed} profile={}",
                        profile.key()
                    ),
                    ingest_oracle::attest_i23(&evidence.expected, &evidence.observed),
                    ingest_oracle::compare_i23(&evidence.expected, &evidence.observed),
                    oracle_records,
                    coverage,
                );
                match fault {
                    None => {
                        if evidence.control.is_some() || !evidence.receipts.is_empty() {
                            return Err(
                                "clean I23 evidence carried fault control or receipts".to_owned()
                            );
                        }
                    }
                    Some(fault) => {
                        let control = evidence.control.as_ref().ok_or_else(|| {
                            format!("I23 fault {fault:?} omitted its same-seed control")
                        })?;
                        if !control.isolated_directories
                            || !control.passed
                            || control.clean_initial_directory != control.fault_initial_directory
                            || control.clean_final != control.fault_final
                        {
                            return Err(format!(
                                "I23 fault {fault:?} failed its byte-identical same-seed relation"
                            ));
                        }
                        let [receipt] = evidence.receipts.as_slice() else {
                            return Err(format!(
                                "I23 fault {fault:?} produced {} typed Store receipts, expected 1",
                                evidence.receipts.len()
                            ));
                        };
                        validate_ingest_receipt(fault, operation, invocation_id, receipt)?;
                        control_records
                            .push(ingest_i23_control_json(seed, operation, fault, control));
                        receipts.push(ProductionFeatureReceipt::ValidatedIngestRetention(
                            receipt.clone(),
                        ));
                    }
                }
            }
        }
    }
    for receipt in &receipts {
        let ProductionFeatureReceipt::ValidatedIngestRetention(receipt) = receipt else {
            return Err("ingest-retention dispatch retained a non-ingest receipt".to_owned());
        };
        coverage.hit(format!(
            "ingest.receipt-site.{}",
            ingest_receipt_site_key(receipt.checkpoint())
        ));
    }
    Ok(receipts)
}

fn record_ingest_family_artifacts(
    operation: super::campaign::IngestOperation,
    fault: Option<ingest_adapter::IngestFaultKind>,
    invocation_id: u64,
    evidence: &ingest_adapter::IngestOperationEvidence,
    family_artifact_records: &mut BTreeMap<&'static str, Vec<String>>,
) -> Result<(), String> {
    for name in super::artifacts::INGEST_REPLAY_ARTIFACTS {
        family_artifact_records.entry(name).or_default();
    }
    let retained = ingest_adapter::RetainedIngestOperationV1::from_evidence(
        ingest_operation_kind(operation),
        fault,
        invocation_id,
        evidence,
    )?;
    family_artifact_records
        .entry("fixture.json")
        .or_default()
        .push(ingest_adapter::retained_ingest_fixture_record_json(
            &retained,
        )?);
    family_artifact_records
        .entry("observations.jsonl")
        .or_default()
        .push(ingest_adapter::retained_ingest_observation_json(
            &retained, evidence,
        )?);
    Ok(())
}

fn ingest_receipt_site_key(checkpoint: IngestRetentionCheckpoint) -> &'static str {
    match checkpoint {
        IngestRetentionCheckpoint::IngestReplayNoWalAppend => "ingest.replay.no-wal-append",
        IngestRetentionCheckpoint::IngestCommitManyAppendError => "ingest.commit-many.append-error",
        IngestRetentionCheckpoint::SealAfterSegmentWriteBeforeManifestCommit => {
            "seal.after-segment-write.before-manifest-commit"
        }
        IngestRetentionCheckpoint::RetentionPolicyEvaluated => "retention.policy-evaluated",
        IngestRetentionCheckpoint::PurgeOldSegmentUnlinkError => "purge.old-segment-unlink.error",
        IngestRetentionCheckpoint::PurgeAfterDurableIntentBeforeRewrite => {
            "purge.after-durable-intent.before-rewrite"
        }
    }
}

fn validate_ingest_receipt(
    fault: ingest_adapter::IngestFaultKind,
    operation: super::campaign::IngestOperation,
    invocation_id: u64,
    receipt: &IngestRetentionFaultReceiptV1,
) -> Result<(), String> {
    let (expected, expected_operation, expected_campaign_operation, expected_checkpoint) =
        match fault {
            ingest_adapter::IngestFaultKind::PostAckRetry => (
                ProductIngestRetentionFaultKind::PostAckRetry,
                ProductIngestRetentionOperation::BatchCommit,
                super::campaign::IngestOperation::BatchCommit,
                IngestRetentionCheckpoint::IngestReplayNoWalAppend,
            ),
            ingest_adapter::IngestFaultKind::PartialBatchAppend => (
                ProductIngestRetentionFaultKind::PartialBatchAppend,
                ProductIngestRetentionOperation::BatchCommit,
                super::campaign::IngestOperation::BatchCommit,
                IngestRetentionCheckpoint::IngestCommitManyAppendError,
            ),
            ingest_adapter::IngestFaultKind::SealCancellation => (
                ProductIngestRetentionFaultKind::SealCancellation,
                ProductIngestRetentionOperation::Seal,
                super::campaign::IngestOperation::Seal,
                IngestRetentionCheckpoint::SealAfterSegmentWriteBeforeManifestCommit,
            ),
            ingest_adapter::IngestFaultKind::RetentionClockBoundary => (
                ProductIngestRetentionFaultKind::RetentionClockBoundary,
                ProductIngestRetentionOperation::Retention,
                super::campaign::IngestOperation::Retention,
                IngestRetentionCheckpoint::RetentionPolicyEvaluated,
            ),
            ingest_adapter::IngestFaultKind::PurgeUnlinkError => (
                ProductIngestRetentionFaultKind::PurgeUnlinkError,
                ProductIngestRetentionOperation::Purge,
                super::campaign::IngestOperation::Purge,
                IngestRetentionCheckpoint::PurgeOldSegmentUnlinkError,
            ),
            ingest_adapter::IngestFaultKind::PurgeCrashBoundary => (
                ProductIngestRetentionFaultKind::PurgeCrashBoundary,
                ProductIngestRetentionOperation::Purge,
                super::campaign::IngestOperation::Purge,
                IngestRetentionCheckpoint::PurgeAfterDurableIntentBeforeRewrite,
            ),
        };
    if receipt.campaign() != "ingest-retention"
        || receipt.operation() != expected_operation
        || operation != expected_campaign_operation
        || receipt.fault() != expected
        || receipt.checkpoint() != expected_checkpoint
        || receipt.cardinality() != 1
        || receipt.invocation_id() != invocation_id
    {
        return Err(format!(
            "ingest-retention receipt header mismatch fault={fault:?} receipt={receipt:?}"
        ));
    }
    match (fault, receipt.effect()) {
        (
            ingest_adapter::IngestFaultKind::PostAckRetry,
            IngestRetentionFaultEffect::PostAckRetry {
                batch_count,
                replay_count,
                returned_seq,
                returned_generation,
                wal_records_appended,
                generation_delta,
                active_published,
            },
        ) if *batch_count > 0
            && batch_count == replay_count
            && *returned_seq > 0
            && *returned_generation > 0
            && *wal_records_appended == 0
            && *generation_delta == 0
            && !*active_published =>
        {
            Ok(())
        }
        (
            ingest_adapter::IngestFaultKind::PartialBatchAppend,
            IngestRetentionFaultEffect::PartialBatchAppend {
                submitted_count,
                changed_records,
                encoded_bytes,
                prefix_bytes,
                io_kind,
                detail,
                active_published,
                generation_delta,
            },
        ) if *submitted_count > 1
            && submitted_count == changed_records
            && *prefix_bytes > 0
            && prefix_bytes < encoded_bytes
            && *io_kind == IngestRetentionIoKind::Other
            && detail
                == &format!(
                    "injected partial-batch-append after {prefix_bytes}/{encoded_bytes} bytes"
                )
            && !*active_published
            && *generation_delta == 0 =>
        {
            Ok(())
        }
        (
            ingest_adapter::IngestFaultKind::SealCancellation,
            IngestRetentionFaultEffect::SealCancellation {
                active_rows,
                absorbed_wal_end,
                candidate_segment,
                manifest_committed,
                temporary_segment_removed,
                generation_delta,
            },
        ) if *active_rows > 0
            && *absorbed_wal_end > 0
            && candidate_segment.iter().any(|byte| *byte != 0)
            && !*manifest_committed
            && *temporary_segment_removed
            && *generation_delta == 0 =>
        {
            Ok(())
        }
        (
            ingest_adapter::IngestFaultKind::RetentionClockBoundary,
            IngestRetentionFaultEffect::RetentionClockBoundary {
                supplied_now,
                window,
                cutoff,
                range_start,
                range_end,
                report_generation,
                dropped_count,
                straddler_count,
                manifest_committed,
            },
        ) if *window > 0
            && *cutoff == supplied_now.saturating_sub(*window)
            && *range_start == i64::MIN
            && *range_end == *cutoff
            && *report_generation > 0
            && *dropped_count == 1
            && *straddler_count == 1
            && *manifest_committed =>
        {
            Ok(())
        }
        (
            ingest_adapter::IngestFaultKind::PurgeUnlinkError,
            IngestRetentionFaultEffect::PurgeUnlinkError {
                original_segment,
                replacement_segment,
                old_file_name,
                io_kind,
                replacement_manifest_committed,
                intent_present,
                old_path_linked,
            },
        ) if original_segment != replacement_segment
            && original_segment.iter().any(|byte| *byte != 0)
            && replacement_segment.iter().any(|byte| *byte != 0)
            && old_file_name
                == &zeppelin_embed::segment::SegmentId::from_bytes(*original_segment)
                    .file_name()
            && *io_kind == IngestRetentionIoKind::Other
            && *replacement_manifest_committed
            && *intent_present
            && *old_path_linked =>
        {
            Ok(())
        }
        (
            ingest_adapter::IngestFaultKind::PurgeCrashBoundary,
            IngestRetentionFaultEffect::PurgeCrashBoundary {
                target_ids,
                token_id,
                intent_file_name,
                intent_durable,
                artifact_rewrites,
                child_aborted,
            },
        ) if !target_ids.is_empty()
            && target_ids
                .iter()
                .zip(target_ids.iter().skip(1))
                .all(|(left, right)| left < right)
            && *token_id > 0
            && intent_file_name == "purge.ze"
            && *intent_durable
            && *artifact_rewrites == 0
            && *child_aborted =>
        {
            Ok(())
        }
        (_, effect) => Err(format!(
            "ingest-retention {fault:?} receipt carried illegal effect facts: {effect:?}"
        )),
    }
}

fn ingest_i21_control_json(
    seed: u64,
    operation: super::campaign::IngestOperation,
    fault: ingest_adapter::IngestFaultKind,
    control: &ingest_adapter::I21ControlEvidence,
) -> String {
    format!(
        "{{\"campaign\":\"ingest-retention\",\"seed\":{seed},\"operation\":\"{}\",\"fault\":\"{}\",\"clean_initial_digest\":\"{:016x}\",\"fault_initial_digest\":\"{:016x}\",\"isolated_directories\":{},\"clean_final_count\":{},\"fault_final_count\":{},\"passed\":{}}}",
        operation.key(),
        ingest_fault_adapter_key(fault),
        control.clean_initial_directory.digest,
        control.fault_initial_directory.digest,
        control.isolated_directories,
        control.clean_final.len(),
        control.fault_final.len(),
        control.passed,
    )
}

pub(crate) fn ingest_retention_control_json_for_evidence(
    seed: u64,
    operation: ingest_adapter::IngestOperationKind,
    fault: Option<ingest_adapter::IngestFaultKind>,
    evidence: &ingest_adapter::IngestOperationEvidence,
) -> Result<Option<String>, String> {
    let Some(fault) = fault else {
        let clean = match evidence {
            ingest_adapter::IngestOperationEvidence::I20(evidence) => {
                evidence.control.is_none() && evidence.receipts.is_empty()
            }
            ingest_adapter::IngestOperationEvidence::I21(evidence) => {
                evidence.control.is_none() && evidence.receipts.is_empty()
            }
            ingest_adapter::IngestOperationEvidence::I22(evidence) => {
                evidence.control.is_none() && evidence.receipts.is_empty()
            }
            ingest_adapter::IngestOperationEvidence::I23(evidence) => {
                evidence.control.is_none() && evidence.receipts.is_empty()
            }
        };
        return clean
            .then_some(None)
            .ok_or_else(|| "clean retained ingest evidence carried fault data".to_owned());
    };
    let campaign_operation = match operation {
        ingest_adapter::IngestOperationKind::BatchCommit => {
            super::campaign::IngestOperation::BatchCommit
        }
        ingest_adapter::IngestOperationKind::Seal => super::campaign::IngestOperation::Seal,
        ingest_adapter::IngestOperationKind::Retention => {
            super::campaign::IngestOperation::Retention
        }
        ingest_adapter::IngestOperationKind::Purge => super::campaign::IngestOperation::Purge,
    };
    let record = match (operation, evidence) {
        (
            ingest_adapter::IngestOperationKind::BatchCommit,
            ingest_adapter::IngestOperationEvidence::I20(evidence),
        ) => ingest_control_json(
            seed,
            campaign_operation,
            fault,
            evidence
                .control
                .as_ref()
                .ok_or_else(|| "retained I20 fault omitted control".to_owned())?,
        ),
        (
            ingest_adapter::IngestOperationKind::Seal,
            ingest_adapter::IngestOperationEvidence::I21(evidence),
        ) => ingest_i21_control_json(
            seed,
            campaign_operation,
            fault,
            evidence
                .control
                .as_ref()
                .ok_or_else(|| "retained I21 fault omitted control".to_owned())?,
        ),
        (
            ingest_adapter::IngestOperationKind::Retention,
            ingest_adapter::IngestOperationEvidence::I22(evidence),
        ) => ingest_i22_control_json(
            seed,
            campaign_operation,
            fault,
            evidence
                .control
                .as_ref()
                .ok_or_else(|| "retained I22 fault omitted control".to_owned())?,
        ),
        (
            ingest_adapter::IngestOperationKind::Purge,
            ingest_adapter::IngestOperationEvidence::I23(evidence),
        ) => ingest_i23_control_json(
            seed,
            campaign_operation,
            fault,
            evidence
                .control
                .as_ref()
                .ok_or_else(|| "retained I23 fault omitted control".to_owned())?,
        ),
        _ => return Err("retained ingest control operation/invariant mismatch".to_owned()),
    };
    Ok(Some(record))
}

fn ingest_i22_control_json(
    seed: u64,
    operation: super::campaign::IngestOperation,
    fault: ingest_adapter::IngestFaultKind,
    control: &ingest_adapter::I22ControlEvidence,
) -> String {
    format!(
        "{{\"campaign\":\"ingest-retention\",\"seed\":{seed},\"operation\":\"{}\",\"fault\":\"{}\",\"clean_initial_digest\":\"{:016x}\",\"fault_initial_digest\":\"{:016x}\",\"isolated_directories\":{},\"clean_final_count\":{},\"fault_final_count\":{},\"passed\":{}}}",
        operation.key(),
        ingest_fault_adapter_key(fault),
        control.clean_initial_directory.digest,
        control.fault_initial_directory.digest,
        control.isolated_directories,
        control.clean_final.len(),
        control.fault_final.len(),
        control.passed,
    )
}

fn ingest_i23_control_json(
    seed: u64,
    operation: super::campaign::IngestOperation,
    fault: ingest_adapter::IngestFaultKind,
    control: &ingest_adapter::I23ControlEvidence,
) -> String {
    format!(
        "{{\"campaign\":\"ingest-retention\",\"seed\":{seed},\"operation\":\"{}\",\"fault\":\"{}\",\"clean_initial_digest\":\"{:016x}\",\"fault_initial_digest\":\"{:016x}\",\"isolated_directories\":{},\"clean_final_count\":{},\"fault_final_count\":{},\"passed\":{}}}",
        operation.key(),
        ingest_fault_adapter_key(fault),
        control.clean_initial_directory.digest,
        control.fault_initial_directory.digest,
        control.isolated_directories,
        control.clean_final.len(),
        control.fault_final.len(),
        control.passed,
    )
}

fn ingest_control_json(
    seed: u64,
    operation: super::campaign::IngestOperation,
    fault: ingest_adapter::IngestFaultKind,
    control: &ingest_adapter::I20ControlEvidence,
) -> String {
    format!(
        "{{\"campaign\":\"ingest-retention\",\"seed\":{seed},\"operation\":\"{}\",\"fault\":\"{}\",\"clean_initial_digest\":\"{:016x}\",\"fault_initial_digest\":\"{:016x}\",\"isolated_directories\":{},\"clean_final_count\":{},\"fault_final_count\":{},\"passed\":{}}}",
        operation.key(),
        ingest_fault_adapter_key(fault),
        control.clean_initial_directory.digest,
        control.fault_initial_directory.digest,
        control.isolated_directories,
        control.clean_final.len(),
        control.fault_final.len(),
        control.passed,
    )
}

fn ingest_fault_adapter_key(fault: ingest_adapter::IngestFaultKind) -> &'static str {
    match fault {
        ingest_adapter::IngestFaultKind::PostAckRetry => "post-ack-retry",
        ingest_adapter::IngestFaultKind::PartialBatchAppend => "partial-batch-append",
        ingest_adapter::IngestFaultKind::SealCancellation => "seal-cancellation",
        ingest_adapter::IngestFaultKind::RetentionClockBoundary => "retention-clock-boundary",
        ingest_adapter::IngestFaultKind::PurgeUnlinkError => "purge-unlink-error",
        ingest_adapter::IngestFaultKind::PurgeCrashBoundary => "purge-crash-boundary",
    }
}

fn storage_fault_kind(
    fault: super::campaign::FeatureFault,
) -> Result<storage_adapter::StorageFaultKind, String> {
    match fault {
        super::campaign::FeatureFault::StorageTornWalHeader => {
            Ok(storage_adapter::StorageFaultKind::TornWalHeader)
        }
        super::campaign::FeatureFault::StorageTornWalBody => {
            Ok(storage_adapter::StorageFaultKind::TornWalBody)
        }
        super::campaign::FeatureFault::StorageTornWalChecksum => {
            Ok(storage_adapter::StorageFaultKind::TornWalChecksum)
        }
        super::campaign::FeatureFault::StoragePostCommitError => {
            Ok(storage_adapter::StorageFaultKind::PostCommitError)
        }
        super::campaign::FeatureFault::StorageManifestPreRenameCrash => {
            Ok(storage_adapter::StorageFaultKind::ManifestPreRenameCrash)
        }
        super::campaign::FeatureFault::StorageManifestPostRenameCrash => {
            Ok(storage_adapter::StorageFaultKind::ManifestPostRenameCrash)
        }
        super::campaign::FeatureFault::StorageCorruptSegmentRegion => {
            Ok(storage_adapter::StorageFaultKind::CorruptSegmentRegion)
        }
        super::campaign::FeatureFault::StorageWrongManifestObject => {
            Ok(storage_adapter::StorageFaultKind::WrongManifestObject)
        }
        super::campaign::FeatureFault::StorageWrongSegmentObject => {
            Ok(storage_adapter::StorageFaultKind::WrongSegmentObject)
        }
        super::campaign::FeatureFault::StorageListDeleteOmission => {
            Ok(storage_adapter::StorageFaultKind::ListDeleteOmission)
        }
        other => Err(format!(
            "storage operation received unrelated fault {}",
            other.key()
        )),
    }
}

fn evidence_hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

fn storage_fixture_column_json(
    column: u32,
    value: &storage_oracle::FixtureColumnValueV1,
) -> String {
    match value {
        storage_oracle::FixtureColumnValueV1::I64(value) => {
            format!("{{\"column\":{column},\"kind\":\"i64\",\"value\":{value}}}")
        }
        storage_oracle::FixtureColumnValueV1::F64Bits(value) => {
            format!("{{\"column\":{column},\"kind\":\"f64-bits\",\"value\":{value}}}")
        }
        storage_oracle::FixtureColumnValueV1::Bool(value) => {
            format!("{{\"column\":{column},\"kind\":\"bool\",\"value\":{value}}}")
        }
        storage_oracle::FixtureColumnValueV1::Bytes(value) => format!(
            "{{\"column\":{column},\"kind\":\"bytes\",\"value\":\"{}\"}}",
            evidence_hex(value)
        ),
    }
}

fn storage_fixture_document_json(value: &storage_oracle::StorageDocumentV1) -> String {
    let vector_bits = value
        .vector_bits
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let timestamp = value
        .timestamp
        .map_or_else(|| "null".to_owned(), |timestamp| timestamp.to_string());
    let metadata = value.metadata.as_ref().map_or_else(
        || "null".to_owned(),
        |metadata| format!("\"{}\"", evidence_hex(metadata)),
    );
    let text = value.text.as_ref().map_or_else(
        || "null".to_owned(),
        |text| format!("\"{}\"", json_escape(text)),
    );
    let columns = value
        .columns
        .iter()
        .map(|(column, value)| storage_fixture_column_json(*column, value))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{{\"doc_id\":\"{}\",\"revision\":{},\"vector_bits\":[{vector_bits}],\"timestamp\":{timestamp},\"metadata_hex\":{metadata},\"text\":{text},\"columns\":[{columns}]}}",
        value.doc_id, value.revision
    )
}

fn storage_fixture_mutation_json(value: &storage_oracle::StorageMutationV1) -> String {
    format!(
        "{{\"operation_id\":\"{}\",\"document_index\":{},\"canonical_payload_digest\":\"{}\",\"first_seq\":{},\"last_seq\":{},\"acknowledged\":{}}}",
        evidence_hex(&value.operation_id),
        value.document_index,
        evidence_hex(&value.canonical_payload_digest),
        value.first_seq,
        value.last_seq,
        value.acknowledged,
    )
}

fn record_storage_family_envelope(
    operation: &str,
    seed: u64,
    envelope: &storage_adapter::StorageEvidenceEnvelope,
    family_artifact_records: &mut BTreeMap<&'static str, Vec<String>>,
) -> Result<(), String> {
    let expected_fixture = storage_oracle::StorageFixtureV1::derive(seed);
    if envelope.fixture != expected_fixture {
        return Err(format!(
            "storage {operation} returned a fixture outside the independent seed derivation"
        ));
    }
    if envelope.artifacts.is_empty() {
        return Err(format!(
            "storage {operation} retained no raw artifact evidence"
        ));
    }
    for acknowledgment in &envelope.ack_ledger {
        let mutation = envelope
            .fixture
            .mutations
            .iter()
            .find(|mutation| mutation.operation_id == acknowledgment.operation_id)
            .ok_or_else(|| {
                format!("storage {operation} acknowledgement names an unknown operation id")
            })?;
        if acknowledgment.canonical_request_digest != mutation.canonical_payload_digest
            || acknowledgment.planned_first_seq != mutation.first_seq
            || acknowledgment.planned_last_seq != mutation.last_seq
            || acknowledgment.durability != envelope.fixture.durability
            || acknowledgment.commit_tier != envelope.fixture.commit_tier
            || acknowledgment.acknowledged != acknowledgment.returned_ok
            || !acknowledgment.returned_ok
                && (acknowledgment.returned_seq.is_some()
                    || acknowledgment.returned_generation.is_some())
            || acknowledgment.returned_ok
                && (acknowledgment.returned_seq.is_none()
                    || acknowledgment.returned_generation.is_none())
        {
            return Err(format!(
                "storage {operation} acknowledgement differs from its independent fixture mutation"
            ));
        }
        family_artifact_records
            .entry("ack-ledger.jsonl")
            .or_default()
            .push(format!(
                "{{\"campaign\":\"storage-durability\",\"operation\":\"{}\",\"seed\":{seed},\"phase\":\"{}\",\"operation_id\":\"{}\",\"canonical_request_digest\":\"{}\",\"planned_first_seq\":{},\"planned_last_seq\":{},\"returned_seq\":{},\"returned_generation\":{},\"returned_ok\":{},\"acknowledged\":{},\"durability\":\"{}\",\"commit_tier\":\"{}\"}}",
                json_escape(operation),
                json_escape(acknowledgment.phase),
                evidence_hex(&acknowledgment.operation_id),
                evidence_hex(&acknowledgment.canonical_request_digest),
                acknowledgment.planned_first_seq,
                acknowledgment.planned_last_seq,
                acknowledgment.returned_seq.map_or_else(|| "null".to_owned(), |value| value.to_string()),
                acknowledgment.returned_generation.map_or_else(|| "null".to_owned(), |value| value.to_string()),
                acknowledgment.returned_ok,
                acknowledgment.acknowledged,
                json_escape(acknowledgment.durability),
                json_escape(acknowledgment.commit_tier),
            ));
    }
    for artifact in &envelope.artifacts {
        if artifact.fact.length
            != u64::try_from(artifact.bytes.len())
                .map_err(|_| "storage artifact byte length exceeds u64".to_owned())?
        {
            return Err(format!(
                "storage artifact {} length fact differs from retained bytes",
                artifact.fact.path
            ));
        }
        let digest = storage_adapter::digest32(0x4649_4c45_4641_4354, &artifact.bytes);
        if artifact.fact.digest != digest {
            return Err(format!(
                "storage artifact {} digest fact differs from retained bytes",
                artifact.fact.path
            ));
        }
        family_artifact_records
            .entry("artifact-index.jsonl")
            .or_default()
            .push(format!(
                "{{\"campaign\":\"storage-durability\",\"operation\":\"{}\",\"seed\":{seed},\"role\":\"{}\",\"path\":\"{}\",\"length\":{},\"digest\":\"{}\",\"bytes_hex\":\"{}\"}}",
                json_escape(operation),
                json_escape(artifact.role),
                json_escape(&artifact.fact.path),
                artifact.fact.length,
                evidence_hex(&artifact.fact.digest),
                evidence_hex(&artifact.bytes),
            ));
    }
    Ok(())
}

fn storage_file_fact_json(fact: &storage_oracle::FileFact) -> String {
    format!(
        "{{\"path\":\"{}\",\"length\":{},\"digest\":\"{}\"}}",
        json_escape(&fact.path),
        fact.length,
        evidence_hex(&fact.digest),
    )
}

fn storage_file_facts_json(facts: &[storage_oracle::FileFact]) -> String {
    facts
        .iter()
        .map(storage_file_fact_json)
        .collect::<Vec<_>>()
        .join(",")
}

fn storage_versions_json(versions: &[(u128, u64)]) -> String {
    versions
        .iter()
        .map(|(document, revision)| {
            format!("{{\"document_id\":\"{document}\",\"revision\":{revision}}}")
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn storage_segment_fact_json(fact: &storage_oracle::SegmentFact) -> String {
    format!(
        "{{\"id\":\"{}\",\"rows\":{},\"scheme\":{},\"dims\":{},\"file_length\":{},\"header_checksum\":{},\"whole_file_checksum\":{}}}",
        evidence_hex(&fact.id),
        fact.rows,
        fact.scheme,
        fact.dims,
        fact.file_length,
        fact.header_checksum,
        fact.whole_file_checksum,
    )
}

fn storage_snapshot_json(snapshot: &storage_oracle::SnapshotState) -> String {
    let segments = snapshot
        .segments
        .iter()
        .map(storage_segment_fact_json)
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{{\"generation\":{},\"manifest_log_seq\":{},\"segments\":[{segments}],\"live_versions\":[{}],\"absorbed_through\":{}}}",
        snapshot.generation,
        snapshot.manifest_log_seq,
        storage_versions_json(&snapshot.live_versions),
        snapshot.absorbed_through,
    )
}

fn storage_publication_model_json(model: &storage_oracle::PublicationModelState) -> String {
    let segments = model
        .segments
        .iter()
        .map(|segment| {
            format!(
                "{{\"id\":\"{}\",\"rows\":{},\"scheme\":{},\"dims\":{},\"file_length\":{},\"header_checksum\":{},\"whole_file_checksum\":{}}}",
                evidence_hex(&segment.id),
                segment.rows,
                segment.scheme,
                segment.dims,
                segment.file_length,
                segment.header_checksum,
                segment.whole_file_checksum,
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{{\"generation\":{},\"manifest_log_seq\":{},\"segments\":[{segments}],\"live_versions\":[{}],\"absorbed_through\":{}}}",
        model.generation,
        model.manifest_log_seq,
        storage_versions_json(&model.live_versions),
        model.absorbed_through,
    )
}

fn storage_published_snapshot_json(snapshot: &storage_oracle::PublishedSnapshotState) -> String {
    let segments = snapshot
        .segments
        .iter()
        .map(|segment| {
            format!(
                "{{\"id\":\"{}\",\"rows\":{},\"scheme\":{},\"dims\":{},\"file_length\":{}}}",
                evidence_hex(&segment.id),
                segment.rows,
                segment.scheme,
                segment.dims,
                segment.file_length,
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{{\"generation\":{},\"segments\":[{segments}],\"live_versions\":[{}],\"absorbed_through\":{}}}",
        snapshot.generation,
        storage_versions_json(&snapshot.live_versions),
        snapshot.absorbed_through,
    )
}

fn storage_publication_class_key(class: storage_oracle::PublicationClass) -> &'static str {
    match class {
        storage_oracle::PublicationClass::Old => "old",
        storage_oracle::PublicationClass::New => "new",
        storage_oracle::PublicationClass::Hybrid => "hybrid",
        storage_oracle::PublicationClass::Invalid => "invalid",
    }
}

fn storage_publication_expected_json(expected: &storage_oracle::PublicationExpected) -> String {
    let legal = expected
        .legal
        .iter()
        .map(|class| format!("\"{}\"", storage_publication_class_key(*class)))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{{\"old\":{},\"new\":{},\"legal\":[{legal}]}}",
        storage_publication_model_json(&expected.old),
        storage_publication_model_json(&expected.new),
    )
}

fn storage_publication_observed_json(observed: &storage_oracle::PublicationObserved) -> String {
    format!(
        "{{\"class\":\"{}\",\"raw\":{},\"public\":{},\"referenced_segments_complete\":{},\"trusted_wal_end\":{}}}",
        storage_publication_class_key(observed.class),
        storage_snapshot_json(&observed.raw),
        storage_published_snapshot_json(&observed.public),
        observed.referenced_segments_complete,
        observed.trusted_wal_end,
    )
}

fn storage_wal_header_failure_json(failure: &storage_oracle::WalHeaderFailure) -> String {
    match failure {
        storage_oracle::WalHeaderFailure::Missing => "{\"kind\":\"missing\"}".to_owned(),
        storage_oracle::WalHeaderFailure::Truncated { needed, available } => {
            format!("{{\"kind\":\"truncated\",\"needed\":{needed},\"available\":{available}}}")
        }
        storage_oracle::WalHeaderFailure::WrongMagic { expected, actual } => format!(
            "{{\"kind\":\"wrong-magic\",\"expected\":\"{}\",\"actual\":\"{}\"}}",
            evidence_hex(expected),
            evidence_hex(actual),
        ),
        storage_oracle::WalHeaderFailure::WrongFamily { expected, actual } => {
            format!("{{\"kind\":\"wrong-family\",\"expected\":{expected},\"actual\":{actual}}}")
        }
        storage_oracle::WalHeaderFailure::UnsupportedVersion {
            family,
            version,
            minimum,
            maximum,
        } => format!(
            "{{\"kind\":\"unsupported-version\",\"family\":{family},\"version\":{version},\"minimum\":{minimum},\"maximum\":{maximum}}}"
        ),
        storage_oracle::WalHeaderFailure::NonZeroFlags { actual } => {
            format!("{{\"kind\":\"nonzero-flags\",\"actual\":{actual}}}")
        }
        storage_oracle::WalHeaderFailure::InvalidHeaderLength { expected, actual } => format!(
            "{{\"kind\":\"invalid-header-length\",\"expected\":{expected},\"actual\":{actual}}}"
        ),
        storage_oracle::WalHeaderFailure::NonZeroFileLength { actual } => {
            format!("{{\"kind\":\"nonzero-file-length\",\"actual\":{actual}}}")
        }
    }
}

fn storage_wal_record_failure_json(failure: &storage_oracle::WalRecordFailure) -> String {
    match failure {
        storage_oracle::WalRecordFailure::HeaderTruncated { needed, available } => format!(
            "{{\"kind\":\"header-truncated\",\"needed\":{needed},\"available\":{available}}}"
        ),
        storage_oracle::WalRecordFailure::BodyTruncated {
            payload_length,
            needed,
            available,
        } => format!(
            "{{\"kind\":\"body-truncated\",\"payload_length\":{payload_length},\"needed\":{needed},\"available\":{available}}}"
        ),
        storage_oracle::WalRecordFailure::ChecksumMismatch {
            expected,
            actual,
            record_length,
        } => format!(
            "{{\"kind\":\"checksum-mismatch\",\"expected\":{expected},\"actual\":{actual},\"record_length\":{record_length}}}"
        ),
        storage_oracle::WalRecordFailure::Sequence { expected, actual } => {
            format!("{{\"kind\":\"sequence\",\"expected\":{expected},\"actual\":{actual}}}")
        }
        storage_oracle::WalRecordFailure::SequenceOverflow { previous } => {
            format!("{{\"kind\":\"sequence-overflow\",\"previous\":{previous}}}")
        }
    }
}

fn storage_corruption_location_key(location: storage_oracle::CorruptionLocation) -> &'static str {
    match location {
        storage_oracle::CorruptionLocation::Tail => "tail",
        storage_oracle::CorruptionLocation::Middle => "middle",
    }
}

fn storage_wal_terminator_json(terminator: &storage_oracle::WalTerminator) -> String {
    match terminator {
        storage_oracle::WalTerminator::CleanEnd => "{\"kind\":\"clean-end\"}".to_owned(),
        storage_oracle::WalTerminator::InvalidHeader { artifact, reason } => format!(
            "{{\"kind\":\"invalid-header\",\"artifact\":\"{}\",\"reason\":{}}}",
            json_escape(artifact),
            storage_wal_header_failure_json(reason),
        ),
        storage_oracle::WalTerminator::CorruptAt {
            artifact,
            offset,
            location,
            reason,
        } => format!(
            "{{\"kind\":\"corrupt-at\",\"artifact\":\"{}\",\"offset\":{offset},\"location\":\"{}\",\"reason\":{}}}",
            json_escape(artifact),
            storage_corruption_location_key(*location),
            storage_wal_record_failure_json(reason),
        ),
    }
}

fn storage_wal_record_json(record: &storage_oracle::WalRecordFact) -> String {
    format!(
        "{{\"seq\":{},\"op\":{},\"payload_hex\":\"{}\"}}",
        record.seq,
        record.op,
        evidence_hex(&record.payload),
    )
}

fn storage_wal_records_json(records: &[storage_oracle::WalRecordFact]) -> String {
    records
        .iter()
        .map(storage_wal_record_json)
        .collect::<Vec<_>>()
        .join(",")
}

fn storage_wal_ack_boundaries_json(boundaries: &[storage_oracle::WalAckBoundary]) -> String {
    boundaries
        .iter()
        .map(|boundary| {
            format!(
                "{{\"records\":[{}],\"live_versions\":[{}]}}",
                storage_wal_records_json(&boundary.records),
                storage_versions_json(&boundary.live_versions),
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn storage_wal_prefix_expected_json(expected: &storage_oracle::WalPrefixExpected) -> String {
    format!(
        "{{\"first_seq\":{},\"acknowledged\":[{}],\"optional_unacknowledged_tail\":[{}],\"terminator\":{},\"live_versions\":[{}],\"ack_boundaries\":[{}]}}",
        expected.first_seq,
        storage_wal_records_json(&expected.acknowledged),
        storage_wal_records_json(&expected.optional_unacknowledged_tail),
        storage_wal_terminator_json(&expected.terminator),
        storage_versions_json(&expected.live_versions),
        storage_wal_ack_boundaries_json(&expected.ack_boundaries),
    )
}

fn storage_wal_public_outcome_json(outcome: &storage_oracle::WalPublicOutcome) -> String {
    match outcome {
        storage_oracle::WalPublicOutcome::Opened { live_versions } => format!(
            "{{\"kind\":\"opened\",\"live_versions\":[{}]}}",
            storage_versions_json(live_versions)
        ),
        storage_oracle::WalPublicOutcome::Refused { terminator } => format!(
            "{{\"kind\":\"refused\",\"terminator\":{}}}",
            storage_wal_terminator_json(terminator)
        ),
    }
}

fn storage_wal_prefix_observed_json(observed: &storage_oracle::WalPrefixObserved) -> String {
    format!(
        "{{\"first_seq\":{},\"records\":[{}],\"terminator\":{},\"clean_public\":{},\"public\":{},\"reopened_ack_boundaries\":[{}]}}",
        observed.first_seq,
        storage_wal_records_json(&observed.records),
        storage_wal_terminator_json(&observed.terminator),
        storage_wal_public_outcome_json(&observed.clean_public),
        storage_wal_public_outcome_json(&observed.public),
        storage_wal_ack_boundaries_json(&observed.reopened_ack_boundaries),
    )
}

fn storage_retry_public_result_json(result: storage_oracle::RetryPublicResult) -> String {
    match result {
        storage_oracle::RetryPublicResult::Committed { seq, generation } => {
            format!("{{\"kind\":\"committed\",\"seq\":{seq},\"generation\":{generation}}}")
        }
        storage_oracle::RetryPublicResult::ScheduledPostCommitError => {
            "{\"kind\":\"scheduled-post-commit-error\"}".to_owned()
        }
    }
}

fn storage_retry_expected_json(expected: &storage_oracle::RetryExpected) -> String {
    format!(
        "{{\"canonical_request_digest\":\"{}\",\"version\":{{\"document_id\":\"{}\",\"revision\":{}}},\"original_seq\":{},\"generation_after_first\":{},\"canonical_record_occurrences\":{},\"ambiguous_first\":{}}}",
        evidence_hex(&expected.canonical_request_digest),
        expected.version.0,
        expected.version.1,
        expected.original_seq,
        expected.generation_after_first,
        expected.canonical_record_occurrences,
        storage_retry_public_result_json(expected.ambiguous_first),
    )
}

fn storage_active_retry_json(observed: &storage_oracle::ActiveRetryObserved) -> String {
    format!(
        "{{\"first\":{},\"retry\":{},\"canonical_record_occurrences\":{},\"live_version_occurrences\":{}}}",
        storage_retry_public_result_json(observed.first),
        storage_retry_public_result_json(observed.retry),
        observed.canonical_record_occurrences,
        observed.live_version_occurrences,
    )
}

fn storage_sealed_retry_json(observed: &storage_oracle::SealedRetryObserved) -> String {
    format!(
        "{{\"retry_seq\":{},\"generation_before_retry\":{},\"generation_after_retry\":{},\"wal_length_before_retry\":{},\"wal_length_after_retry\":{},\"wal_digest_before_retry\":\"{}\",\"wal_digest_after_retry\":\"{}\",\"live_version_occurrences\":{}}}",
        observed.retry_seq,
        observed.generation_before_retry,
        observed.generation_after_retry,
        observed.wal_length_before_retry,
        observed.wal_length_after_retry,
        evidence_hex(&observed.wal_digest_before_retry),
        evidence_hex(&observed.wal_digest_after_retry),
        observed.live_version_occurrences,
    )
}

fn storage_retry_observed_json(observed: &storage_oracle::RetryObserved) -> String {
    let sealed = observed
        .sealed_reopened
        .as_ref()
        .map_or_else(|| "null".to_owned(), storage_sealed_retry_json);
    format!(
        "{{\"canonical_request_digest\":\"{}\",\"version\":{{\"document_id\":\"{}\",\"revision\":{}}},\"first_seq\":{},\"retry_seq\":{},\"generation_before_retry\":{},\"generation_after_retry\":{},\"wal_length_before_retry\":{},\"wal_length_after_retry\":{},\"wal_digest_before_retry\":\"{}\",\"wal_digest_after_retry\":\"{}\",\"canonical_record_occurrences\":{},\"live_version_occurrences\":{},\"ambiguous_first\":{},\"active_same_handle\":{},\"sealed_reopened\":{sealed}}}",
        evidence_hex(&observed.canonical_request_digest),
        observed.version.0,
        observed.version.1,
        observed.first_seq,
        observed.retry_seq,
        observed.generation_before_retry,
        observed.generation_after_retry,
        observed.wal_length_before_retry,
        observed.wal_length_after_retry,
        evidence_hex(&observed.wal_digest_before_retry),
        evidence_hex(&observed.wal_digest_after_retry),
        observed.canonical_record_occurrences,
        observed.live_version_occurrences,
        storage_retry_public_result_json(observed.ambiguous_first),
        storage_active_retry_json(&observed.active_same_handle),
    )
}

fn storage_artifact_fact_json(artifact: &storage_oracle::ArtifactFact) -> String {
    match artifact {
        storage_oracle::ArtifactFact::Wal { path } => {
            format!("{{\"kind\":\"wal\",\"path\":\"{}\"}}", json_escape(path))
        }
        storage_oracle::ArtifactFact::Manifest { path } => format!(
            "{{\"kind\":\"manifest\",\"path\":\"{}\"}}",
            json_escape(path)
        ),
        storage_oracle::ArtifactFact::Segment { path, id } => {
            let id = id.as_ref().map_or_else(
                || "null".to_owned(),
                |id| format!("\"{}\"", evidence_hex(id)),
            );
            format!(
                "{{\"kind\":\"segment\",\"path\":\"{}\",\"id\":{id}}}",
                json_escape(path)
            )
        }
        storage_oracle::ArtifactFact::SegmentRegion {
            path,
            id,
            kind,
            chunk,
        } => format!(
            "{{\"kind\":\"segment-region\",\"path\":\"{}\",\"id\":\"{}\",\"region_kind\":{kind},\"chunk\":{chunk}}}",
            json_escape(path),
            evidence_hex(id),
        ),
    }
}

fn storage_format_check_key(check: storage_oracle::FormatCheckFact) -> &'static str {
    match check {
        storage_oracle::FormatCheckFact::Length => "length",
        storage_oracle::FormatCheckFact::Magic => "magic",
        storage_oracle::FormatCheckFact::Family => "family",
        storage_oracle::FormatCheckFact::Version => "version",
        storage_oracle::FormatCheckFact::HeaderLength => "header-length",
        storage_oracle::FormatCheckFact::FileLength => "file-length",
        storage_oracle::FormatCheckFact::BlockLength => "block-length",
        storage_oracle::FormatCheckFact::BlockChecksum => "block-checksum",
        storage_oracle::FormatCheckFact::FileChecksum => "file-checksum",
        storage_oracle::FormatCheckFact::ObjectIdentity => "object-identity",
    }
}

fn storage_format_call_key(call: storage_oracle::FormatPublicCall) -> &'static str {
    match call {
        storage_oracle::FormatPublicCall::Open => "open",
        storage_oracle::FormatPublicCall::ExactSearch => "exact-search",
    }
}

fn storage_format_case_key(case: storage_oracle::FormatCase) -> &'static str {
    match case {
        storage_oracle::FormatCase::WalHeader => "wal-header",
        storage_oracle::FormatCase::WalRecordBody => "wal-record-body",
        storage_oracle::FormatCase::WalRecordChecksum => "wal-record-checksum",
        storage_oracle::FormatCase::SegmentRegion => "segment-region",
        storage_oracle::FormatCase::ManifestWrongFamily => "manifest-wrong-family",
        storage_oracle::FormatCase::SegmentWrongFamily => "segment-wrong-family",
        storage_oracle::FormatCase::SegmentWrongIdentity => "segment-wrong-identity",
    }
}

fn storage_omission_case_key(
    evidence: &storage_adapter::ReachabilityOperationEvidence,
) -> Result<Option<&'static str>, String> {
    let Some(receipt) = evidence.receipt_observed.as_ref() else {
        return Ok(None);
    };
    if !evidence
        .expected
        .eligible_orphans
        .iter()
        .any(|file| file.path == evidence.mutation.artifact)
    {
        return Err(format!(
            "storage omission target {} is not independently classified as eligible",
            evidence.mutation.artifact
        ));
    }
    let orphan = if evidence.mutation.artifact == ".manifest.ze.tmp" {
        "manifest-temporary"
    } else if evidence.mutation.artifact.starts_with(".segment-")
        && evidence.mutation.artifact.ends_with(".zseg.tmp")
    {
        "segment-temporary"
    } else if evidence.mutation.artifact.starts_with("segment-")
        && evidence.mutation.artifact.ends_with(".zseg")
    {
        "final-segment"
    } else {
        return Err(format!(
            "storage omission target {} has no eligible orphan kind",
            evidence.mutation.artifact
        ));
    };
    let subsite = match receipt.value.site {
        storage_oracle::ReceiptSite::OrphanCleanupList => "list",
        storage_oracle::ReceiptSite::OrphanCleanupDelete => "delete",
        ref site => {
            return Err(format!(
                "storage omission receipt used the wrong production site: {site:?}"
            ));
        }
    };
    match (orphan, subsite) {
        ("final-segment", "list") => Ok(Some("storage.omission.final-segment.list")),
        ("final-segment", "delete") => Ok(Some("storage.omission.final-segment.delete")),
        ("segment-temporary", "list") => Ok(Some("storage.omission.segment-temporary.list")),
        ("segment-temporary", "delete") => Ok(Some("storage.omission.segment-temporary.delete")),
        ("manifest-temporary", "list") => Ok(Some("storage.omission.manifest-temporary.list")),
        ("manifest-temporary", "delete") => Ok(Some("storage.omission.manifest-temporary.delete")),
        _ => Err("storage omission case was not closed over the catalog".to_owned()),
    }
}

fn storage_format_refusal_json(refusal: &storage_oracle::FormatRefusal) -> String {
    match refusal {
        storage_oracle::FormatRefusal::WalInvalidHeader { artifact, reason } => format!(
            "{{\"kind\":\"wal-invalid-header\",\"artifact\":{},\"reason\":{}}}",
            storage_artifact_fact_json(artifact),
            storage_wal_header_failure_json(reason),
        ),
        storage_oracle::FormatRefusal::WalCorruptAt {
            artifact,
            offset,
            location,
            reason,
        } => format!(
            "{{\"kind\":\"wal-corrupt-at\",\"artifact\":{},\"offset\":{offset},\"location\":\"{}\",\"reason\":{}}}",
            storage_artifact_fact_json(artifact),
            storage_corruption_location_key(*location),
            storage_wal_record_failure_json(reason),
        ),
        storage_oracle::FormatRefusal::ManifestFormat {
            artifact,
            check,
            offset,
            expected,
            actual,
        } => format!(
            "{{\"kind\":\"manifest-format\",\"artifact\":{},\"check\":\"{}\",\"offset\":{offset},\"expected\":{expected},\"actual\":{actual}}}",
            storage_artifact_fact_json(artifact),
            storage_format_check_key(*check),
        ),
        storage_oracle::FormatRefusal::SegmentFormat {
            artifact,
            check,
            offset,
            expected,
            actual,
        } => format!(
            "{{\"kind\":\"segment-format\",\"artifact\":{},\"check\":\"{}\",\"offset\":{offset},\"expected\":{expected},\"actual\":{actual}}}",
            storage_artifact_fact_json(artifact),
            storage_format_check_key(*check),
        ),
        storage_oracle::FormatRefusal::SegmentWrongObject {
            artifact,
            expected,
            actual,
        } => format!(
            "{{\"kind\":\"segment-wrong-object\",\"artifact\":{},\"expected\":\"{}\",\"actual\":\"{}\"}}",
            storage_artifact_fact_json(artifact),
            evidence_hex(expected),
            evidence_hex(actual),
        ),
    }
}

fn storage_format_expected_json(expected: &storage_oracle::FormatExpected) -> String {
    let case = storage_format_case_key(expected.case);
    format!(
        "{{\"case\":\"{case}\",\"call\":\"{}\",\"clean\":{},\"refusal\":{}}}",
        storage_format_call_key(expected.call),
        storage_format_clean_outcome_json(expected.clean),
        storage_format_refusal_json(&expected.refusal),
    )
}

fn storage_format_clean_outcome_json(outcome: storage_oracle::FormatCleanOutcome) -> String {
    match outcome {
        storage_oracle::FormatCleanOutcome::Opened => "{\"kind\":\"opened\"}".to_owned(),
        storage_oracle::FormatCleanOutcome::ExactSearch { candidates } => {
            format!("{{\"kind\":\"exact-search\",\"candidates\":{candidates}}}")
        }
    }
}

fn storage_format_observed_json(observed: &storage_oracle::FormatObserved) -> String {
    let case = storage_format_case_key(observed.case);
    let refusal = observed
        .refusal
        .as_ref()
        .map_or_else(|| "null".to_owned(), storage_format_refusal_json);
    format!(
        "{{\"case\":\"{case}\",\"call\":\"{}\",\"clean\":{},\"refusal\":{refusal},\"partial_candidates\":{}}}",
        storage_format_call_key(observed.call),
        storage_format_clean_outcome_json(observed.clean),
        observed.partial_candidates,
    )
}

fn storage_reachability_expected_json(expected: &storage_oracle::ReachabilityExpected) -> String {
    format!(
        "{{\"baseline\":[{}],\"manifest_referenced\":[{}],\"control_and_unknown\":[{}],\"preserved\":[{}],\"eligible_orphans\":[{}],\"reclaimed_bytes\":{},\"directory_sync_required\":{},\"expected_directory_syncs\":{},\"committed_purge_read_only_required\":{}}}",
        storage_file_facts_json(&expected.baseline),
        storage_file_facts_json(&expected.manifest_referenced),
        storage_file_facts_json(&expected.control_and_unknown),
        storage_file_facts_json(&expected.preserved),
        storage_file_facts_json(&expected.eligible_orphans),
        expected.reclaimed_bytes,
        expected.directory_sync_required,
        expected.expected_directory_syncs,
        expected.committed_purge_read_only_required,
    )
}

fn storage_committed_purge_read_only_json(
    observed: &storage_oracle::CommittedPurgeReadOnlyObserved,
) -> String {
    let outcome = match observed.outcome {
        storage_oracle::CommittedPurgeReadOnlyOutcome::RefusedPurgeRecoveryReadOnly => {
            "refused-purge-recovery-read-only"
        }
    };
    format!(
        "{{\"before\":[{}],\"after\":[{}],\"outcome\":\"{outcome}\"}}",
        storage_file_facts_json(&observed.before),
        storage_file_facts_json(&observed.after),
    )
}

fn storage_reachability_observed_json(observed: &storage_oracle::ReachabilityObserved) -> String {
    let committed_purge_read_only = observed
        .committed_purge_read_only
        .as_ref()
        .map_or_else(|| "null".to_owned(), storage_committed_purge_read_only_json);
    format!(
        "{{\"after_read_only\":[{}],\"final_inventory\":[{}],\"reclaimed_bytes\":{},\"directory_syncs\":{},\"committed_purge_read_only\":{committed_purge_read_only}}}",
        storage_file_facts_json(&observed.after_read_only),
        storage_file_facts_json(&observed.final_inventory),
        observed.reclaimed_bytes,
        observed.directory_syncs,
    )
}

fn storage_mutation_json(mutation: &storage_adapter::StorageMutationEvidence) -> String {
    let optional_u64 =
        |value: Option<u64>| value.map_or_else(|| "null".to_owned(), |value| value.to_string());
    let optional_u32 =
        |value: Option<u32>| value.map_or_else(|| "null".to_owned(), |value| value.to_string());
    let optional_u16 =
        |value: Option<u16>| value.map_or_else(|| "null".to_owned(), |value| value.to_string());
    let optional_u8 =
        |value: Option<u8>| value.map_or_else(|| "null".to_owned(), |value| value.to_string());
    let segment = mutation.segment.as_ref().map_or_else(
        || "null".to_owned(),
        |segment| format!("\"{}\"", evidence_hex(segment)),
    );
    format!(
        "{{\"artifact\":\"{}\",\"offset\":{},\"segment\":{segment},\"region_kind\":{},\"chunk\":{},\"before\":{},\"after\":{}}}",
        json_escape(&mutation.artifact),
        optional_u64(mutation.offset),
        optional_u16(mutation.region_kind),
        optional_u32(mutation.chunk),
        optional_u8(mutation.before),
        optional_u8(mutation.after),
    )
}

fn storage_artifact_evidence_json(artifact: &storage_adapter::StorageArtifactEvidence) -> String {
    format!(
        "{{\"role\":\"{}\",\"fact\":{},\"bytes_hex\":\"{}\"}}",
        json_escape(artifact.role),
        storage_file_fact_json(&artifact.fact),
        evidence_hex(&artifact.bytes),
    )
}

fn storage_control_json(control: &storage_adapter::StorageControlEvidence) -> String {
    let artifacts = |values: &[storage_adapter::StorageArtifactEvidence]| {
        values
            .iter()
            .map(storage_artifact_evidence_json)
            .collect::<Vec<_>>()
            .join(",")
    };
    format!(
        "{{\"namespace\":\"{}\",\"seed\":{},\"operation_fixture_id\":\"{}\",\"clean_fault_pair_id\":\"{}\",\"pre_clean_inventory\":[{}],\"pre_fault_inventory\":[{}],\"pre_clean_digest\":\"{}\",\"pre_fault_digest\":\"{}\",\"pre_clean_artifacts\":[{}],\"pre_fault_artifacts\":[{}],\"clean_inventory\":[{}],\"fault_inventory\":[{}],\"clean_digest\":\"{}\",\"fault_digest\":\"{}\"}}",
        json_escape(control.namespace),
        control.seed,
        evidence_hex(&control.operation_fixture_id),
        evidence_hex(&control.clean_fault_pair_id),
        storage_file_facts_json(&control.pre_clean_inventory),
        storage_file_facts_json(&control.pre_fault_inventory),
        evidence_hex(&control.pre_clean_digest),
        evidence_hex(&control.pre_fault_digest),
        artifacts(&control.pre_clean_artifacts),
        artifacts(&control.pre_fault_artifacts),
        storage_file_facts_json(&control.clean_inventory),
        storage_file_facts_json(&control.fault_inventory),
        evidence_hex(&control.clean_digest),
        evidence_hex(&control.fault_digest),
    )
}

fn storage_receipt_operation_key(operation: storage_oracle::ReceiptOperation) -> &'static str {
    match operation {
        storage_oracle::ReceiptOperation::WalPrefix => "wal-prefix",
        storage_oracle::ReceiptOperation::Publication => "publication",
        storage_oracle::ReceiptOperation::Retry => "retry",
        storage_oracle::ReceiptOperation::FormatCheck => "format-check",
        storage_oracle::ReceiptOperation::OrphanCleanup => "orphan-cleanup",
    }
}

fn storage_receipt_fault_key(fault: storage_oracle::ReceiptFault) -> &'static str {
    match fault {
        storage_oracle::ReceiptFault::TornWalHeader => "torn-wal-header",
        storage_oracle::ReceiptFault::TornWalBody => "torn-wal-body",
        storage_oracle::ReceiptFault::TornWalChecksum => "torn-wal-checksum",
        storage_oracle::ReceiptFault::PostCommitError => "post-commit-error",
        storage_oracle::ReceiptFault::ManifestPreRenameCrash => "manifest-pre-rename-crash",
        storage_oracle::ReceiptFault::ManifestPostRenameCrash => "manifest-post-rename-crash",
        storage_oracle::ReceiptFault::CorruptSegmentRegion => "corrupt-segment-region",
        storage_oracle::ReceiptFault::WrongManifestObject => "wrong-manifest-object",
        storage_oracle::ReceiptFault::WrongSegmentObject => "wrong-segment-object",
        storage_oracle::ReceiptFault::ListDeleteOmission => "list-delete-omission",
    }
}

fn storage_receipt_site_key(site: storage_oracle::ReceiptSite) -> &'static str {
    match site {
        storage_oracle::ReceiptSite::WalOpenHeaderValidation => "wal-open-header-validation",
        storage_oracle::ReceiptSite::WalOpenRecordValidation => "wal-open-record-validation",
        storage_oracle::ReceiptSite::WalOpenRecordChecksum => "wal-open-record-checksum",
        storage_oracle::ReceiptSite::WalCommitAppendAfterInnerSuccess => {
            "wal-commit-append-after-inner-success"
        }
        storage_oracle::ReceiptSite::ManifestCommitBeforeRename => "manifest-commit-before-rename",
        storage_oracle::ReceiptSite::ManifestCommitAfterRename => "manifest-commit-after-rename",
        storage_oracle::ReceiptSite::SegmentReadRegionChecksum => "segment-read-region-checksum",
        storage_oracle::ReceiptSite::ManifestOpenFamilyValidation => {
            "manifest-open-family-validation"
        }
        storage_oracle::ReceiptSite::SegmentOpenFamilyValidation => {
            "segment-open-family-validation"
        }
        storage_oracle::ReceiptSite::SegmentOpenObjectIdentity => "segment-open-object-identity",
        storage_oracle::ReceiptSite::OrphanCleanupList => "orphan-cleanup-list",
        storage_oracle::ReceiptSite::OrphanCleanupDelete => "orphan-cleanup-delete",
    }
}

fn storage_receipt_effect_json(effect: &storage_oracle::ReceiptEffectFact) -> String {
    let optional_u16 =
        |value: Option<u16>| value.map_or_else(|| "null".to_owned(), |value| value.to_string());
    let optional_id = |value: &Option<[u8; 16]>| {
        value.as_ref().map_or_else(
            || "null".to_owned(),
            |value| format!("\"{}\"", evidence_hex(value)),
        )
    };
    match effect {
        storage_oracle::ReceiptEffectFact::WalHeader {
            planned_offset,
            retained_len,
            observed,
        } => format!(
            "{{\"kind\":\"wal-header\",\"planned_offset\":{planned_offset},\"retained_len\":{retained_len},\"observed\":{}}}",
            storage_wal_header_failure_json(observed)
        ),
        storage_oracle::ReceiptEffectFact::WalRecord {
            planned_offset,
            observed_offset,
            location,
            observed,
        } => format!(
            "{{\"kind\":\"wal-record\",\"planned_offset\":{planned_offset},\"observed_offset\":{observed_offset},\"location\":\"{}\",\"observed\":{}}}",
            storage_corruption_location_key(*location),
            storage_wal_record_failure_json(observed),
        ),
        storage_oracle::ReceiptEffectFact::WalAppend {
            encoded_len,
            first_seq,
            last_seq,
            inner_append_completed,
            caller_saw_error,
        } => format!(
            "{{\"kind\":\"wal-append\",\"encoded_len\":{encoded_len},\"first_seq\":{first_seq},\"last_seq\":{last_seq},\"inner_append_completed\":{inner_append_completed},\"caller_saw_error\":{caller_saw_error}}}"
        ),
        storage_oracle::ReceiptEffectFact::ManifestRename {
            temporary,
            committed,
            rename_performed,
            new_segment_final,
            directory_sync_returned,
        } => format!(
            "{{\"kind\":\"manifest-rename\",\"temporary\":\"{}\",\"committed\":\"{}\",\"rename_performed\":{rename_performed},\"new_segment_final\":{new_segment_final},\"directory_sync_returned\":{directory_sync_returned}}}",
            json_escape(temporary),
            json_escape(committed),
        ),
        storage_oracle::ReceiptEffectFact::SegmentChecksum {
            segment,
            region_kind,
            chunk,
            expected_checksum,
            actual_checksum,
        } => format!(
            "{{\"kind\":\"segment-checksum\",\"segment\":\"{}\",\"region_kind\":{region_kind},\"chunk\":{chunk},\"expected_checksum\":{expected_checksum},\"actual_checksum\":{actual_checksum}}}",
            evidence_hex(segment),
        ),
        storage_oracle::ReceiptEffectFact::Format {
            artifact,
            check,
            expected_family,
            actual_family,
            expected_id,
            actual_id,
        } => format!(
            "{{\"kind\":\"format\",\"artifact\":{},\"check\":\"{}\",\"expected_family\":{},\"actual_family\":{},\"expected_id\":{},\"actual_id\":{}}}",
            storage_artifact_fact_json(artifact),
            storage_format_check_key(*check),
            optional_u16(*expected_family),
            optional_u16(*actual_family),
            optional_id(expected_id),
            optional_id(actual_id),
        ),
        storage_oracle::ReceiptEffectFact::Omission {
            omitted_path,
            deletion_observed,
        } => format!(
            "{{\"kind\":\"omission\",\"omitted_path\":\"{}\",\"deletion_observed\":{deletion_observed}}}",
            json_escape(omitted_path),
        ),
    }
}

fn storage_receipt_expected_json(expected: &storage_oracle::ReceiptExpected) -> String {
    format!(
        "{{\"campaign\":\"{}\",\"operation\":\"{}\",\"fault\":\"{}\",\"site\":\"{}\",\"op_index\":{},\"artifact\":{},\"effect\":{}}}",
        json_escape(expected.campaign),
        storage_receipt_operation_key(expected.operation),
        storage_receipt_fault_key(expected.fault),
        storage_receipt_site_key(expected.site),
        expected.op_index,
        storage_artifact_fact_json(&expected.artifact),
        storage_receipt_effect_json(&expected.effect),
    )
}

fn storage_optional_receipt_expected_json(
    expected: Option<&storage_oracle::ReceiptExpected>,
) -> String {
    expected.map_or_else(|| "null".to_owned(), storage_receipt_expected_json)
}

fn storage_optional_receipt_observed_json(
    observed: Option<&storage_oracle::ReceiptObserved>,
) -> String {
    observed.map_or_else(
        || "null".to_owned(),
        |observed| {
            format!(
                "{{\"value\":{},\"cardinality\":{}}}",
                storage_receipt_expected_json(&observed.value),
                observed.cardinality,
            )
        },
    )
}

fn storage_ack_json(acknowledgment: &storage_adapter::StorageAckEvidence) -> String {
    let optional =
        |value: Option<u64>| value.map_or_else(|| "null".to_owned(), |value| value.to_string());
    format!(
        "{{\"phase\":\"{}\",\"operation_id\":\"{}\",\"canonical_request_digest\":\"{}\",\"planned_first_seq\":{},\"planned_last_seq\":{},\"returned_seq\":{},\"returned_generation\":{},\"returned_ok\":{},\"acknowledged\":{},\"durability\":\"{}\",\"commit_tier\":\"{}\"}}",
        json_escape(acknowledgment.phase),
        evidence_hex(&acknowledgment.operation_id),
        evidence_hex(&acknowledgment.canonical_request_digest),
        acknowledgment.planned_first_seq,
        acknowledgment.planned_last_seq,
        optional(acknowledgment.returned_seq),
        optional(acknowledgment.returned_generation),
        acknowledgment.returned_ok,
        acknowledgment.acknowledged,
        json_escape(acknowledgment.durability),
        json_escape(acknowledgment.commit_tier),
    )
}

fn storage_ack_detail_json(acknowledgments: &[storage_adapter::StorageAckEvidence]) -> String {
    let acknowledgments = acknowledgments
        .iter()
        .map(storage_ack_json)
        .collect::<Vec<_>>()
        .join(",");
    format!("{{\"kind\":\"ack-ledger\",\"acknowledgments\":[{acknowledgments}]}}")
}

fn storage_fault_kind_key(fault: storage_adapter::StorageFaultKind) -> &'static str {
    match fault {
        storage_adapter::StorageFaultKind::TornWalHeader => "torn-wal-header",
        storage_adapter::StorageFaultKind::TornWalBody => "torn-wal-body",
        storage_adapter::StorageFaultKind::TornWalChecksum => "torn-wal-checksum",
        storage_adapter::StorageFaultKind::PostCommitError => "post-commit-error",
        storage_adapter::StorageFaultKind::ManifestPreRenameCrash => "manifest-pre-rename-crash",
        storage_adapter::StorageFaultKind::ManifestPostRenameCrash => "manifest-post-rename-crash",
        storage_adapter::StorageFaultKind::CorruptSegmentRegion => "corrupt-segment-region",
        storage_adapter::StorageFaultKind::WrongManifestObject => "wrong-manifest-object",
        storage_adapter::StorageFaultKind::WrongSegmentObject => "wrong-segment-object",
        storage_adapter::StorageFaultKind::ListDeleteOmission => "list-delete-omission",
    }
}

fn storage_child_detail_json(child: Option<&storage_adapter::ChildAbortEvidence>) -> String {
    child.map_or_else(
        || "{\"kind\":\"child-abort\",\"child\":null}".to_owned(),
        |child| {
            format!(
                "{{\"kind\":\"child-abort\",\"child\":{{\"signal\":{},\"acknowledgment\":\"{}\",\"fault\":\"{}\",\"site\":\"{}\",\"op_index\":{},\"artifact\":\"{}\",\"temporary\":\"{}\",\"committed\":\"{}\",\"rename_performed\":{},\"new_segment_final\":{},\"directory_sync_returned\":{}}}}}",
                child.signal,
                json_escape(&child.acknowledgment),
                storage_fault_kind_key(child.fault),
                json_escape(child.site),
                child.op_index,
                json_escape(&child.artifact),
                json_escape(&child.temporary),
                json_escape(&child.committed),
                child.rename_performed,
                child.new_segment_final,
                child.directory_sync_returned,
            )
        },
    )
}

fn storage_cleanup_json(cleanup: &StorageCleanupReport) -> String {
    let deleted = cleanup
        .deleted_paths()
        .iter()
        .map(|path| format!("\"{}\"", json_escape(path)))
        .collect::<Vec<_>>()
        .join(",");
    let retained = cleanup
        .retained_eligible_paths()
        .iter()
        .map(|path| format!("\"{}\"", json_escape(path)))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{{\"reclaimed_bytes\":{},\"deleted_paths\":[{deleted}],\"retained_eligible_paths\":[{retained}],\"directory_synced\":{}}}",
        cleanup.reclaimed_bytes(),
        cleanup.directory_synced(),
    )
}

fn storage_omission_detail_json(
    omission: Option<&storage_adapter::OmissionIntermediateEvidence>,
) -> String {
    omission.map_or_else(
        || "{\"kind\":\"omission\",\"omission\":null}".to_owned(),
        |omission| {
            format!(
                "{{\"kind\":\"omission\",\"omission\":{{\"inventory\":[{}],\"cleanup\":{},\"retry_cleanup\":{}}}}}",
                storage_file_facts_json(&omission.inventory),
                storage_cleanup_json(&omission.cleanup),
                storage_cleanup_json(&omission.retry_cleanup),
            )
        },
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "one storage comparison owns all exact replay evidence streams"
)]
fn record_storage_evidence<T: std::fmt::Debug, U: std::fmt::Debug, E: std::fmt::Display>(
    invariant: u8,
    checker_id: &'static str,
    operation: &'static str,
    _expected: &T,
    _observed: &U,
    checker_result: Result<(), E>,
    receipt_expected: Option<&storage_oracle::ReceiptExpected>,
    receipt_observed: Option<&storage_oracle::ReceiptObserved>,
    receipt: Option<StorageFaultReceipt>,
    control: &storage_adapter::StorageControlEvidence,
    mutation: &storage_adapter::StorageMutationEvidence,
    evidence: &storage_adapter::StorageEvidenceEnvelope,
    expected_json: String,
    observed_json: String,
    operation_detail_json: String,
    seed: u64,
    profile: FaultProfile,
    op_index: usize,
    oracle_records: &mut Vec<OracleRecord>,
    control_records: &mut Vec<String>,
    mutation_records: &mut Vec<String>,
    family_artifact_records: &mut BTreeMap<&'static str, Vec<String>>,
    coverage: &mut CoverageRegistry,
    receipts: &mut Vec<ProductionFeatureReceipt>,
) -> Result<bool, String> {
    record_storage_family_envelope(operation, seed, evidence, family_artifact_records)?;
    if control.pre_clean_inventory != control.pre_fault_inventory
        || control.pre_clean_digest != control.pre_fault_digest
    {
        return Err(format!(
            "storage {operation} clean/fault fixtures were not byte-identical before the operation"
        ));
    }
    let provenance = format!(
        "{} seed={seed} profile={} op={op_index} operation={operation}",
        storage_oracle::ORACLE_CONTRACT_VERSION,
        profile.key()
    );
    let invariant_result = checker_result.map_err(|error| error.to_string());
    let receipt_result = match receipt_expected {
        Some(expected_receipt) => match (receipt.as_ref(), receipt_observed) {
            (Some(raw_receipt), Some(adapter_observed)) => {
                storage_adapter::receipt_observed_from_product(raw_receipt).and_then(
                    |raw_observed| {
                        if &raw_observed != adapter_observed {
                            Err(format!(
                                "{checker_id}: adapter receipt DTO differs from exhaustive raw production translation: adapter={adapter_observed:?} raw={raw_observed:?}"
                            ))
                        } else {
                            storage_oracle::check_receipt(
                                checker_id,
                                expected_receipt,
                                Some(&raw_observed),
                            )
                            .map_err(|error| error.to_string())
                        }
                    },
                )
            }
            (None, _) => Err(format!(
                "{checker_id}: raw production receipt missing while receipt attestation was required"
            )),
            (Some(_), None) => Err(format!(
                "{checker_id}: adapter receipt DTO missing while a raw production receipt was supplied"
            )),
        },
        None => {
            if receipt_observed.is_some() || receipt.is_some() {
                return Err(format!(
                    "storage {operation} emitted an unscheduled production receipt"
                ));
            }
            Ok(())
        }
    };
    let receipt_valid = receipt_result.is_ok();
    let combined_result = match (invariant_result, receipt_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(invariant), Ok(())) => Err(invariant),
        (Ok(()), Err(receipt)) => Err(format!("receipt attestation failed: {receipt}")),
        (Err(invariant), Err(receipt)) => Err(format!(
            "invariant comparison failed: {invariant}; receipt attestation failed: {receipt}"
        )),
    };
    let comparison_passed = combined_result.is_ok();
    let comparison_error = combined_result.as_ref().err().cloned();
    push_feature_json_record(
        invariant,
        checker_id,
        operation,
        format!(
            "{{\"value\":{expected_json},\"receipt\":{}}}",
            storage_optional_receipt_expected_json(receipt_expected)
        ),
        format!(
            "{{\"value\":{observed_json},\"receipt\":{}}}",
            storage_optional_receipt_observed_json(receipt_observed)
        ),
        provenance,
        combined_result,
        oracle_records,
        coverage,
    );
    if comparison_passed
        && receipt_valid
        && let Some(observed) = receipt_observed
    {
        coverage.hit(format!(
            "storage.receipt-site.{}",
            storage_receipt_site_key(observed.value.site)
        ));
        let subcase = match observed.value.site {
            storage_oracle::ReceiptSite::SegmentOpenFamilyValidation => {
                Some("feature_fault.storage-durability.wrong-segment-object.site.family")
            }
            storage_oracle::ReceiptSite::SegmentOpenObjectIdentity => {
                Some("feature_fault.storage-durability.wrong-segment-object.site.identity")
            }
            storage_oracle::ReceiptSite::OrphanCleanupList => {
                Some("feature_fault.storage-durability.list-delete-omission.site.list")
            }
            storage_oracle::ReceiptSite::OrphanCleanupDelete => {
                Some("feature_fault.storage-durability.list-delete-omission.site.delete")
            }
            _ => None,
        };
        if let Some(subcase) = subcase {
            coverage.hit(subcase);
        }
    }
    if comparison_passed
        && receipt_valid
        && let Some(receipt) = receipt
    {
        family_artifact_records
            .entry("feature-receipts.jsonl")
            .or_default()
            .push(production_receipt_json(
                &ProductionFeatureReceipt::ValidatedStorage(receipt.clone()),
            ));
        receipts.push(ProductionFeatureReceipt::ValidatedStorage(receipt));
    }
    family_artifact_records
        .entry("storage-observations.jsonl")
        .or_default()
        .push(format!(
            "{{\"campaign\":\"storage-durability\",\"operation\":\"{}\",\"seed\":{seed},\"expected\":{expected_json},\"observed\":{observed_json},\"receipt_expected\":{},\"receipt_observed\":{},\"operation_detail\":{operation_detail_json}}}",
            json_escape(operation),
            storage_optional_receipt_expected_json(receipt_expected),
            storage_optional_receipt_observed_json(receipt_observed),
        ));
    let control_record = format!(
        "{{\"campaign\":\"storage-durability\",\"operation\":\"{}\",\"seed\":{seed},\"control\":{}}}",
        json_escape(operation),
        storage_control_json(control),
    );
    control_records.push(control_record.clone());
    family_artifact_records
        .entry("clean-controls.jsonl")
        .or_default()
        .push(control_record);
    let mutation_record = format!(
        "{{\"campaign\":\"storage-durability\",\"operation\":\"{}\",\"seed\":{seed},\"mutation\":{}}}",
        json_escape(operation),
        storage_mutation_json(mutation),
    );
    mutation_records.push(mutation_record);
    match comparison_error {
        Some(error) => Err(format!(
            "storage {operation} exact checker/receipt attestation failed: {error}"
        )),
        None => Ok(comparison_passed),
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the WAL refusal projection binds its exact oracle and replay streams"
)]
fn record_storage_wal_i18_projection(
    evidence: &storage_adapter::WalPrefixOperationEvidence,
    seed: u64,
    profile: FaultProfile,
    op_index: usize,
    oracle_records: &mut Vec<OracleRecord>,
    family_artifact_records: &mut BTreeMap<&'static str, Vec<String>>,
    coverage: &mut CoverageRegistry,
) -> Result<(), String> {
    let (expected, observed) = storage_adapter::format_dtos_from_wal_prefix(evidence)?;
    let invariant_result =
        storage_oracle::check_i18(&expected, &observed).map_err(|error| error.to_string());
    let receipt_result = match (
        evidence.receipt_expected.as_ref(),
        evidence.receipt_observed.as_ref(),
        evidence.receipt.as_ref(),
    ) {
        (Some(expected_receipt), Some(adapter_observed), Some(raw_receipt)) => {
            storage_adapter::receipt_observed_from_product(raw_receipt).and_then(|raw_observed| {
                if &raw_observed != adapter_observed {
                    Err(format!(
                        "{}: adapter receipt DTO differs from exhaustive raw production translation: adapter={adapter_observed:?} raw={raw_observed:?}",
                        storage_oracle::I18_CHECKER_ID,
                    ))
                } else {
                    storage_oracle::check_receipt(
                        storage_oracle::I18_CHECKER_ID,
                        expected_receipt,
                        Some(&raw_observed),
                    )
                    .map_err(|error| error.to_string())
                }
            })
        }
        (None, None, None) => Err(format!(
            "{}: damaged WAL projection omitted its production receipt",
            storage_oracle::I18_CHECKER_ID,
        )),
        _ => Err(format!(
            "{}: damaged WAL projection receipt evidence is incomplete",
            storage_oracle::I18_CHECKER_ID,
        )),
    };
    let combined_result = match (invariant_result, receipt_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(invariant), Ok(())) => Err(invariant),
        (Ok(()), Err(receipt)) => Err(format!("receipt attestation failed: {receipt}")),
        (Err(invariant), Err(receipt)) => Err(format!(
            "invariant comparison failed: {invariant}; receipt attestation failed: {receipt}"
        )),
    };
    let passed = combined_result.is_ok();
    let comparison_error = combined_result.as_ref().err().cloned();
    let expected_json = storage_format_expected_json(&expected);
    let observed_json = storage_format_observed_json(&observed);
    push_feature_json_record(
        18,
        storage_oracle::I18_CHECKER_ID,
        "wal-prefix",
        format!(
            "{{\"value\":{expected_json},\"receipt\":{}}}",
            storage_optional_receipt_expected_json(evidence.receipt_expected.as_ref())
        ),
        format!(
            "{{\"value\":{observed_json},\"receipt\":{}}}",
            storage_optional_receipt_observed_json(evidence.receipt_observed.as_ref())
        ),
        format!(
            "{} seed={seed} profile={} op={op_index} operation=wal-prefix projection=I18",
            storage_oracle::ORACLE_CONTRACT_VERSION,
            profile.key(),
        ),
        combined_result,
        oracle_records,
        coverage,
    );
    if passed {
        coverage.hit(format!(
            "storage.format-case.{}",
            storage_format_case_key(expected.case)
        ));
    }
    family_artifact_records
        .entry("storage-observations.jsonl")
        .or_default()
        .push(format!(
            "{{\"campaign\":\"storage-durability\",\"operation\":\"wal-prefix\",\"projection\":\"I18\",\"seed\":{seed},\"expected\":{expected_json},\"observed\":{observed_json},\"receipt_expected\":{},\"receipt_observed\":{}}}",
            storage_optional_receipt_expected_json(evidence.receipt_expected.as_ref()),
            storage_optional_receipt_observed_json(evidence.receipt_observed.as_ref()),
        ));
    match comparison_error {
        Some(error) => Err(format!(
            "storage wal-prefix I18 projection checker/receipt attestation failed: {error}"
        )),
        None => Ok(()),
    }
}

#[cfg(test)]
mod storage_receipt_credit_tests {
    use super::*;

    #[test]
    fn raw_storage_artifact_bytes_must_match_the_independent_digest_fact() {
        let mut evidence = storage_adapter::observe_retry(0x51, 2, None)
            .expect("observe storage artifact evidence");
        let artifact = evidence
            .evidence
            .artifacts
            .iter_mut()
            .find(|artifact| !artifact.bytes.is_empty())
            .expect("nonempty storage artifact");
        artifact.bytes[0] ^= 1;
        let mut records = BTreeMap::new();
        let error = record_storage_family_envelope("retry", 0x51, &evidence.evidence, &mut records)
            .expect_err("mutated raw storage bytes were accepted");
        assert!(error.contains("digest"), "{error}");
    }

    #[test]
    fn storage_ack_ledger_must_match_the_independent_fixture_mutation() {
        let mut evidence = storage_adapter::observe_retry(0x52, 2, None)
            .expect("observe storage acknowledgement evidence");
        let acknowledgment = evidence
            .evidence
            .ack_ledger
            .first_mut()
            .expect("storage acknowledgement");
        acknowledgment.canonical_request_digest[0] ^= 1;
        let mut records = BTreeMap::new();
        let error = record_storage_family_envelope("retry", 0x52, &evidence.evidence, &mut records)
            .expect_err("mutated storage acknowledgement was accepted");
        assert!(error.contains("acknowledgement"), "{error}");
    }

    #[test]
    fn mismatched_storage_receipt_cannot_earn_fault_or_duplicate_invariant_credit() {
        let evidence = storage_adapter::observe_retry(
            0x17,
            3,
            Some(storage_adapter::StorageFaultKind::PostCommitError),
        )
        .expect("observe storage retry fault");
        let mut receipt_observed = evidence
            .receipt_observed
            .clone()
            .expect("retry fault receipt observation");
        receipt_observed.cardinality = 2;
        let mut oracle_records = Vec::new();
        let mut control_records = Vec::new();
        let mut mutation_records = Vec::new();
        let mut family_artifact_records = BTreeMap::new();
        let mut coverage = CoverageRegistry::default();
        let mut receipts = Vec::new();

        let error = record_storage_evidence(
            17,
            storage_oracle::I17_CHECKER_ID,
            "retry-idempotence",
            &evidence.expected,
            &evidence.observed,
            storage_oracle::check_i17(&evidence.expected, &evidence.observed),
            evidence.receipt_expected.as_ref(),
            Some(&receipt_observed),
            evidence.receipt,
            &evidence.control,
            &evidence.mutation,
            &evidence.evidence,
            storage_retry_expected_json(&evidence.expected),
            storage_retry_observed_json(&evidence.observed),
            "{\"kind\":\"retry\"}".to_owned(),
            0x17,
            FaultProfile::None,
            3,
            &mut oracle_records,
            &mut control_records,
            &mut mutation_records,
            &mut family_artifact_records,
            &mut coverage,
            &mut receipts,
        )
        .expect_err("mismatched storage receipt returned operation success");
        assert!(error.contains("receipt attestation failed"), "{error}");

        assert_eq!(
            oracle_records.len(),
            1,
            "receipt check duplicated I17 credit"
        );
        assert!(
            !oracle_records[0].passed,
            "receipt mismatch did not fail I17"
        );
        assert!(
            receipts.is_empty(),
            "mismatched receipt earned fault credit"
        );
    }

    #[test]
    fn failed_storage_invariant_cannot_earn_receipt_or_site_credit() {
        let evidence = storage_adapter::observe_retry(
            0x18,
            3,
            Some(storage_adapter::StorageFaultKind::PostCommitError),
        )
        .expect("observe storage retry fault");
        let mut oracle_records = Vec::new();
        let mut control_records = Vec::new();
        let mut mutation_records = Vec::new();
        let mut family_artifact_records = BTreeMap::new();
        let mut coverage = CoverageRegistry::default();
        let mut receipts = Vec::new();

        let error = record_storage_evidence(
            17,
            storage_oracle::I17_CHECKER_ID,
            "retry-idempotence",
            &evidence.expected,
            &evidence.observed,
            Err::<(), _>("planted I17 mismatch"),
            evidence.receipt_expected.as_ref(),
            evidence.receipt_observed.as_ref(),
            evidence.receipt,
            &evidence.control,
            &evidence.mutation,
            &evidence.evidence,
            storage_retry_expected_json(&evidence.expected),
            storage_retry_observed_json(&evidence.observed),
            "{\"kind\":\"retry\"}".to_owned(),
            0x18,
            FaultProfile::None,
            3,
            &mut oracle_records,
            &mut control_records,
            &mut mutation_records,
            &mut family_artifact_records,
            &mut coverage,
            &mut receipts,
        )
        .expect_err("failed I17 returned operation success");

        assert!(error.contains("planted I17 mismatch"), "{error}");
        assert!(
            receipts.is_empty(),
            "failed I17 earned a production receipt"
        );
        assert_eq!(
            coverage.count("storage.receipt-site.wal-commit-append-after-inner-success"),
            0,
            "failed I17 earned receipt-site coverage"
        );
    }

    #[test]
    fn failed_wal_i18_projection_cannot_return_operation_success() {
        let mut evidence = storage_adapter::observe_wal_prefix(
            0x19,
            1,
            Some(storage_adapter::StorageFaultKind::TornWalHeader),
        )
        .expect("observe damaged WAL projection");
        evidence
            .receipt_observed
            .as_mut()
            .expect("damaged WAL receipt")
            .cardinality = 2;
        let mut oracle_records = Vec::new();
        let mut family_artifact_records = BTreeMap::new();
        let mut coverage = CoverageRegistry::default();

        let error = record_storage_wal_i18_projection(
            &evidence,
            0x19,
            FaultProfile::None,
            1,
            &mut oracle_records,
            &mut family_artifact_records,
            &mut coverage,
        )
        .expect_err("failed WAL I18 projection returned operation success");
        assert!(error.contains("receipt attestation failed"), "{error}");
        assert_eq!(
            coverage.count("storage.format-case.wal-header"),
            0,
            "failed WAL I18 projection earned format coverage"
        );
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the shared adapter boundary carries every replay evidence stream"
)]
fn storage_episode_fixture_json(episode: &storage_adapter::StorageEpisodeFixtures) -> String {
    let base = episode.base_evidence();
    let acknowledgments = base
        .ack_ledger
        .iter()
        .map(storage_ack_json)
        .collect::<Vec<_>>()
        .join(",");
    let artifacts = base
        .artifacts
        .iter()
        .map(storage_artifact_evidence_json)
        .collect::<Vec<_>>()
        .join(",");
    let base_json = format!(
        "{{\"bootstrap_document_count\":{},\"ack_ledger\":[{acknowledgments}],\"reopened_ack_boundaries\":[{}],\"snapshot\":{},\"referenced_segments_complete\":{},\"public_versions\":[{}],\"inventory\":[{}],\"inventory_digest\":\"{}\",\"artifacts\":[{artifacts}]}}",
        base.bootstrap_document_count,
        storage_wal_ack_boundaries_json(&base.reopened_ack_boundaries),
        storage_snapshot_json(&base.snapshot),
        base.referenced_segments_complete,
        storage_versions_json(&base.public_versions),
        storage_file_facts_json(&base.inventory),
        evidence_hex(&base.inventory_digest),
    );
    let forks = episode
        .operation_fixtures()
        .iter()
        .map(|fixture| {
            let traversal = fixture
                .evidence
                .traversal
                .iter()
                .map(|path| format!("\"{}\"", json_escape(path)))
                .collect::<Vec<_>>()
                .join(",");
            let artifacts = fixture
                .evidence
                .destination_artifacts
                .iter()
                .map(storage_artifact_evidence_json)
                .collect::<Vec<_>>()
                .join(",");
            format!(
                "{{\"operation\":\"{}\",\"traversal\":[{traversal}],\"source_inventory\":[{}],\"destination_inventory\":[{}],\"source_digest\":\"{}\",\"destination_digest\":\"{}\",\"destination_artifacts\":[{artifacts}]}}",
                fixture.evidence.operation.key(),
                storage_file_facts_json(&fixture.evidence.source_inventory),
                storage_file_facts_json(&fixture.evidence.destination_inventory),
                evidence_hex(&fixture.evidence.source_digest),
                evidence_hex(&fixture.evidence.destination_digest),
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("\"episode_base\":{base_json},\"operation_forks\":[{forks}]")
}

const fn storage_retained_operation_kind(
    operation: super::campaign::StorageOperation,
) -> storage_adapter::StorageOperationKind {
    match operation {
        super::campaign::StorageOperation::WalPrefix => {
            storage_adapter::StorageOperationKind::WalPrefix
        }
        super::campaign::StorageOperation::Publication => {
            storage_adapter::StorageOperationKind::Publication
        }
        super::campaign::StorageOperation::Retry => storage_adapter::StorageOperationKind::Retry,
        super::campaign::StorageOperation::FormatCheck => {
            storage_adapter::StorageOperationKind::FormatCheck
        }
        super::campaign::StorageOperation::OrphanCleanup => {
            storage_adapter::StorageOperationKind::OrphanCleanup
        }
    }
}

fn storage_retained_format_case(
    operation: super::campaign::StorageOperation,
    seed: u64,
    fault: Option<storage_adapter::StorageFaultKind>,
) -> Option<storage_oracle::FormatCase> {
    match operation {
        super::campaign::StorageOperation::WalPrefix => match fault {
            Some(storage_adapter::StorageFaultKind::TornWalHeader) => {
                Some(storage_oracle::FormatCase::WalHeader)
            }
            Some(storage_adapter::StorageFaultKind::TornWalBody) => {
                Some(storage_oracle::FormatCase::WalRecordBody)
            }
            Some(storage_adapter::StorageFaultKind::TornWalChecksum) => {
                Some(storage_oracle::FormatCase::WalRecordChecksum)
            }
            _ => None,
        },
        super::campaign::StorageOperation::FormatCheck => Some(match fault {
            Some(storage_adapter::StorageFaultKind::CorruptSegmentRegion) => {
                storage_oracle::FormatCase::SegmentRegion
            }
            Some(storage_adapter::StorageFaultKind::WrongManifestObject) => {
                storage_oracle::FormatCase::ManifestWrongFamily
            }
            Some(storage_adapter::StorageFaultKind::WrongSegmentObject) if seed & 1 == 1 => {
                storage_oracle::FormatCase::SegmentWrongIdentity
            }
            Some(storage_adapter::StorageFaultKind::WrongSegmentObject) => {
                storage_oracle::FormatCase::SegmentWrongFamily
            }
            None => match seed % 4 {
                0 => storage_oracle::FormatCase::SegmentRegion,
                1 => storage_oracle::FormatCase::ManifestWrongFamily,
                2 => storage_oracle::FormatCase::SegmentWrongFamily,
                _ => storage_oracle::FormatCase::SegmentWrongIdentity,
            },
            Some(_) => return None,
        }),
        super::campaign::StorageOperation::Publication
        | super::campaign::StorageOperation::Retry
        | super::campaign::StorageOperation::OrphanCleanup => None,
    }
}

fn storage_retained_omission_case(
    operation: super::campaign::StorageOperation,
    seed: u64,
    profile: FaultProfile,
    fault: Option<storage_adapter::StorageFaultKind>,
) -> Result<Option<storage_oracle::OmissionCase>, String> {
    if operation != super::campaign::StorageOperation::OrphanCleanup
        || fault != Some(storage_adapter::StorageFaultKind::ListDeleteOmission)
    {
        return Ok(None);
    }
    let profile_ordinal = FaultProfile::ALL
        .iter()
        .position(|candidate| *candidate == profile)
        .and_then(|ordinal| u32::try_from(ordinal).ok())
        .ok_or_else(|| "storage profile ordinal is absent".to_owned())?;
    let schedule_seed = seed.saturating_add(seed / FaultProfile::ALL.len() as u64);
    Ok(Some(storage_adapter::omission_case_for_schedule(
        schedule_seed,
        profile_ordinal,
    )))
}

#[allow(
    clippy::too_many_arguments,
    reason = "retained storage identity includes its exact operation, fault, and subcase"
)]
fn record_storage_retained_fixture(
    fixture: &storage_oracle::StorageFixtureV1,
    operation: super::campaign::StorageOperation,
    op_index: u32,
    fault: Option<storage_adapter::StorageFaultKind>,
    format_case: Option<storage_oracle::FormatCase>,
    omission_case: Option<storage_oracle::OmissionCase>,
    family_artifact_records: &mut BTreeMap<&'static str, Vec<String>>,
) -> Result<(), String> {
    let retained = storage_adapter::RetainedStorageOperationV1::new(
        fixture.clone(),
        storage_retained_operation_kind(operation),
        op_index,
        fault,
        format_case,
        omission_case,
    )?;
    let retained = storage_adapter::encode_storage_fixture(&retained)?;
    let records = family_artifact_records
        .get_mut("storage-fixture.json")
        .ok_or_else(|| "storage fixture artifact stream is absent".to_owned())?;
    let record = records
        .first_mut()
        .ok_or_else(|| "storage fixture base record is absent".to_owned())?;
    let mut fixture_json: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_str(record)
            .map_err(|error| format!("parse storage fixture artifact: {error}"))?;
    let operations = fixture_json["retained_operations"]
        .as_array_mut()
        .ok_or_else(|| "storage fixture retained_operations is absent".to_owned())?;
    let retained_hex = evidence_hex(&retained);
    if operations
        .iter()
        .any(|entry| entry["retained_fixture_hex"].as_str() == Some(retained_hex.as_str()))
    {
        return Err(format!(
            "storage fixture retained operation {} with the same fault/subcase was recorded twice",
            operation.key()
        ));
    }
    operations.push(zeppelin_embed_bench::harness_json::json!({
        "schema": storage_adapter::STORAGE_RETAINED_FIXTURE_SCHEMA,
        "operation": operation.key(),
        "op_index": op_index,
        "retained_fixture_hex": retained_hex,
    }));
    *record = fixture_json.to_string();
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "the shared adapter boundary carries every replay evidence stream"
)]
fn run_storage_campaign_operation(
    operation: super::campaign::StorageOperation,
    selected_faults: &[super::campaign::FeatureFault],
    episode: &storage_adapter::StorageEpisodeFixtures,
    seed: u64,
    profile: FaultProfile,
    op_index: usize,
    oracle_records: &mut Vec<OracleRecord>,
    control_records: &mut Vec<String>,
    mutation_records: &mut Vec<String>,
    family_artifact_records: &mut BTreeMap<&'static str, Vec<String>>,
    coverage: &mut CoverageRegistry,
) -> Result<Vec<ProductionFeatureReceipt>, String> {
    for name in super::artifacts::STORAGE_REPLAY_ARTIFACTS {
        family_artifact_records.entry(name).or_default();
    }
    let fixture = storage_oracle::StorageFixtureV1::derive(seed);
    let eligible_orphans = fixture
        .eligible_orphans
        .iter()
        .map(|path| format!("\"{}\"", json_escape(path)))
        .collect::<Vec<_>>()
        .join(",");
    let preserved_files = fixture
        .preserved_files
        .iter()
        .map(|path| format!("\"{}\"", json_escape(path)))
        .collect::<Vec<_>>()
        .join(",");
    let documents = fixture
        .documents
        .iter()
        .map(storage_fixture_document_json)
        .collect::<Vec<_>>()
        .join(",");
    let mutations = fixture
        .mutations
        .iter()
        .map(storage_fixture_mutation_json)
        .collect::<Vec<_>>()
        .join(",");
    let episode_fixture = storage_episode_fixture_json(episode);
    let fixture_record = format!(
        "{{\"campaign\":\"storage-durability\",\"namespace\":\"{}\",\"seed\":{seed},\"durability\":\"{}\",\"commit_tier\":\"{}\",\"dimensions\":{},\"scheme\":{},\"documents\":[{documents}],\"mutations\":[{mutations}],\"old_generation\":{},\"planned_new_generation\":{},\"absorbed_through\":{},\"wal_mutation_offset\":{},\"segment_region_kind\":{},\"segment_chunk\":{},\"segment_byte\":{},\"omission_is_delete\":{},\"eligible_orphans\":[{eligible_orphans}],\"preserved_files\":[{preserved_files}],\"clean_fault_pair_id\":\"{}\",\"operation_fixture_id\":\"{}\",\"retained_operations\":[],{episode_fixture}}}",
        json_escape(fixture.namespace),
        json_escape(fixture.durability),
        json_escape(fixture.commit_tier),
        fixture.dimensions,
        fixture.scheme,
        fixture.old_generation,
        fixture.planned_new_generation,
        fixture.absorbed_through,
        fixture.wal_mutation_offset,
        fixture.segment_region_kind,
        fixture.segment_chunk,
        fixture.segment_byte,
        fixture.omission_is_delete,
        evidence_hex(&fixture.clean_fault_pair_id),
        evidence_hex(&fixture.operation_fixture_id),
    );
    let fixture_records = family_artifact_records
        .get_mut("storage-fixture.json")
        .ok_or_else(|| "storage fixture artifact stream is absent".to_owned())?;
    match fixture_records.as_slice() {
        [] => fixture_records.push(fixture_record),
        [existing] => {
            let mut existing: zeppelin_embed_bench::harness_json::Value =
                zeppelin_embed_bench::harness_json::from_str(existing)
                    .map_err(|error| format!("parse existing storage fixture: {error}"))?;
            existing["retained_operations"] = zeppelin_embed_bench::harness_json::json!([]);
            let expected: zeppelin_embed_bench::harness_json::Value =
                zeppelin_embed_bench::harness_json::from_str(&fixture_record)
                    .map_err(|error| format!("parse expected storage fixture: {error}"))?;
            if existing != expected {
                return Err("storage fixture artifact changed within one episode".to_owned());
            }
        }
        _ => return Err("storage fixture artifact changed within one episode".to_owned()),
    }
    let op_index_u32 =
        u32::try_from(op_index).map_err(|_| "storage operation index exceeds u32".to_owned())?;
    let matching_faults = selected_faults
        .iter()
        .copied()
        .filter(|fault| fault.operation() == super::campaign::FeatureOperation::Storage(operation))
        .map(storage_fault_kind)
        .map(|fault| fault.map(Some))
        .collect::<Result<Vec<_>, _>>()?;
    let cases = if matching_faults.is_empty() {
        vec![None]
    } else {
        matching_faults
    };
    let mut receipts = Vec::new();
    for fault in cases {
        let format_case = storage_retained_format_case(operation, seed, fault);
        let omission_case = storage_retained_omission_case(operation, seed, profile, fault)?;
        record_storage_retained_fixture(
            &fixture,
            operation,
            op_index_u32,
            fault,
            format_case,
            omission_case,
            family_artifact_records,
        )?;
        match operation {
            super::campaign::StorageOperation::WalPrefix => {
                let evidence =
                    storage_adapter::observe_wal_prefix_from_episode(episode, op_index_u32, fault)?;
                let has_i18_projection = matches!(
                    fault,
                    Some(
                        storage_adapter::StorageFaultKind::TornWalHeader
                            | storage_adapter::StorageFaultKind::TornWalBody
                            | storage_adapter::StorageFaultKind::TornWalChecksum
                    )
                );
                record_storage_evidence(
                    16,
                    storage_oracle::I16_CHECKER_ID,
                    operation.key(),
                    &evidence.expected,
                    &evidence.observed,
                    storage_oracle::check_i16(&evidence.expected, &evidence.observed),
                    evidence.receipt_expected.as_ref(),
                    evidence.receipt_observed.as_ref(),
                    evidence.receipt.clone(),
                    &evidence.control,
                    &evidence.mutation,
                    &evidence.evidence,
                    storage_wal_prefix_expected_json(&evidence.expected),
                    storage_wal_prefix_observed_json(&evidence.observed),
                    storage_ack_detail_json(&evidence.ack_ledger),
                    seed,
                    profile,
                    op_index,
                    oracle_records,
                    control_records,
                    mutation_records,
                    family_artifact_records,
                    coverage,
                    &mut receipts,
                )?;
                if has_i18_projection {
                    record_storage_wal_i18_projection(
                        &evidence,
                        seed,
                        profile,
                        op_index,
                        oracle_records,
                        family_artifact_records,
                        coverage,
                    )?;
                }
            }
            super::campaign::StorageOperation::Publication => {
                #[cfg(unix)]
                let evidence = storage_adapter::observe_publication_from_episode(
                    episode,
                    op_index_u32,
                    fault,
                    storage_adapter::PUBLICATION_CHILD_TEST_NAME,
                )?;
                #[cfg(not(unix))]
                return Err("storage publication crash adapter requires Unix".to_owned());
                #[cfg(unix)]
                {
                    if evidence.child.is_some() {
                        coverage.hit("op.crash");
                    }
                    record_storage_evidence(
                        15,
                        storage_oracle::I15_CHECKER_ID,
                        operation.key(),
                        &evidence.expected,
                        &evidence.observed,
                        storage_oracle::check_i15(&evidence.expected, &evidence.observed),
                        evidence.receipt_expected.as_ref(),
                        evidence.receipt_observed.as_ref(),
                        evidence.receipt,
                        &evidence.control,
                        &evidence.mutation,
                        &evidence.evidence,
                        storage_publication_expected_json(&evidence.expected),
                        storage_publication_observed_json(&evidence.observed),
                        storage_child_detail_json(evidence.child.as_ref()),
                        seed,
                        profile,
                        op_index,
                        oracle_records,
                        control_records,
                        mutation_records,
                        family_artifact_records,
                        coverage,
                        &mut receipts,
                    )?;
                }
            }
            super::campaign::StorageOperation::Retry => {
                let evidence =
                    storage_adapter::observe_retry_from_episode(episode, op_index_u32, fault)?;
                record_storage_evidence(
                    17,
                    storage_oracle::I17_CHECKER_ID,
                    operation.key(),
                    &evidence.expected,
                    &evidence.observed,
                    storage_oracle::check_i17(&evidence.expected, &evidence.observed),
                    evidence.receipt_expected.as_ref(),
                    evidence.receipt_observed.as_ref(),
                    evidence.receipt,
                    &evidence.control,
                    &evidence.mutation,
                    &evidence.evidence,
                    storage_retry_expected_json(&evidence.expected),
                    storage_retry_observed_json(&evidence.observed),
                    "{\"kind\":\"retry\"}".to_owned(),
                    seed,
                    profile,
                    op_index,
                    oracle_records,
                    control_records,
                    mutation_records,
                    family_artifact_records,
                    coverage,
                    &mut receipts,
                )?;
            }
            super::campaign::StorageOperation::FormatCheck => {
                let evidence =
                    storage_adapter::observe_format_from_episode(episode, op_index_u32, fault)?;
                let format_case = evidence.expected.case;
                let passed = record_storage_evidence(
                    18,
                    storage_oracle::I18_CHECKER_ID,
                    operation.key(),
                    &evidence.expected,
                    &evidence.observed,
                    storage_oracle::check_i18(&evidence.expected, &evidence.observed),
                    evidence.receipt_expected.as_ref(),
                    evidence.receipt_observed.as_ref(),
                    evidence.receipt,
                    &evidence.control,
                    &evidence.mutation,
                    &evidence.evidence,
                    storage_format_expected_json(&evidence.expected),
                    storage_format_observed_json(&evidence.observed),
                    "{\"kind\":\"format-check\"}".to_owned(),
                    seed,
                    profile,
                    op_index,
                    oracle_records,
                    control_records,
                    mutation_records,
                    family_artifact_records,
                    coverage,
                    &mut receipts,
                )?;
                if passed {
                    coverage.hit(format!(
                        "storage.format-case.{}",
                        storage_format_case_key(format_case)
                    ));
                }
            }
            super::campaign::StorageOperation::OrphanCleanup => {
                let evidence = if fault
                    == Some(storage_adapter::StorageFaultKind::ListDeleteOmission)
                {
                    let profile_ordinal = FaultProfile::ALL
                        .iter()
                        .position(|candidate| *candidate == profile)
                        .and_then(|ordinal| u32::try_from(ordinal).ok())
                        .ok_or_else(|| "storage profile ordinal is absent".to_owned())?;
                    let schedule_seed = seed.saturating_add(seed / FaultProfile::ALL.len() as u64);
                    let omission_case =
                        storage_adapter::omission_case_for_schedule(schedule_seed, profile_ordinal);
                    storage_adapter::observe_reachability_case_from_episode(
                        episode,
                        op_index_u32,
                        Some(omission_case),
                    )?
                } else {
                    storage_adapter::observe_reachability_from_episode(
                        episode,
                        op_index_u32,
                        fault,
                    )?
                };
                let omission_case = storage_omission_case_key(&evidence)?;
                let passed = record_storage_evidence(
                    19,
                    storage_oracle::I19_CHECKER_ID,
                    operation.key(),
                    &evidence.expected,
                    &evidence.observed,
                    storage_oracle::check_i19(&evidence.expected, &evidence.observed),
                    evidence.receipt_expected.as_ref(),
                    evidence.receipt_observed.as_ref(),
                    evidence.receipt,
                    &evidence.control,
                    &evidence.mutation,
                    &evidence.evidence,
                    storage_reachability_expected_json(&evidence.expected),
                    storage_reachability_observed_json(&evidence.observed),
                    storage_omission_detail_json(evidence.omission.as_ref()),
                    seed,
                    profile,
                    op_index,
                    oracle_records,
                    control_records,
                    mutation_records,
                    family_artifact_records,
                    coverage,
                    &mut receipts,
                )?;
                if passed && let Some(case) = omission_case {
                    coverage.hit(case);
                }
            }
        }
        coverage.hit("op.ingest");
    }
    Ok(receipts)
}

fn metadata_operation_kind(
    operation: super::campaign::MetadataOperation,
) -> metadata_adapter::MetadataOperationKind {
    match operation {
        super::campaign::MetadataOperation::Columns => {
            metadata_adapter::MetadataOperationKind::Columns
        }
        super::campaign::MetadataOperation::Bitmap => {
            metadata_adapter::MetadataOperationKind::Bitmap
        }
        super::campaign::MetadataOperation::Planner => {
            metadata_adapter::MetadataOperationKind::Planner
        }
        super::campaign::MetadataOperation::Execution => {
            metadata_adapter::MetadataOperationKind::Execution
        }
    }
}

fn metadata_fault_kind(
    fault: super::campaign::FeatureFault,
) -> Result<metadata_adapter::MetadataFaultKind, String> {
    match fault {
        super::campaign::FeatureFault::MetadataColumnCorruption => {
            Ok(metadata_adapter::MetadataFaultKind::ColumnCorruption)
        }
        super::campaign::FeatureFault::MetadataBitmapTruncation => {
            Ok(metadata_adapter::MetadataFaultKind::BitmapTruncation)
        }
        super::campaign::FeatureFault::MetadataSelectivityBoundary => {
            Ok(metadata_adapter::MetadataFaultKind::SelectivityBoundary)
        }
        super::campaign::FeatureFault::MetadataVisitedBudgetFallback => {
            Ok(metadata_adapter::MetadataFaultKind::VisitedBudgetFallback)
        }
        other => Err(format!(
            "metadata operation received unrelated fault {}",
            other.key()
        )),
    }
}

fn metadata_source_label(source: RowSource) -> String {
    match source {
        RowSource::Active => "active".to_owned(),
        RowSource::Sealed(id) => format!("sealed-{}", id.file_name()),
    }
}

fn metadata_provenance_matches(
    expected: &metadata_adapter::MetadataProvenanceExpected,
    observed: &MetadataDecodeProvenance,
) -> bool {
    match (expected, observed) {
        (
            metadata_adapter::MetadataProvenanceExpected::ColumnsPresenceTail {
                column_id: expected_column,
                row_count: expected_rows,
                byte_offset: expected_offset,
                observed_byte: expected_byte,
                allowed_mask: expected_mask,
            },
            MetadataDecodeProvenance::ColumnsPresenceTail {
                column_id,
                row_count,
                byte_offset,
                observed_byte,
                allowed_mask,
            },
        ) => {
            expected_column == column_id
                && expected_rows == row_count
                && expected_offset == byte_offset
                && expected_byte == observed_byte
                && expected_mask == allowed_mask
        }
        (
            metadata_adapter::MetadataProvenanceExpected::ColumnsDictionaryCode {
                column_id: expected_column,
                row: expected_row,
                byte_offset: expected_offset,
                code: expected_code,
                dictionary_len: expected_len,
            },
            MetadataDecodeProvenance::ColumnsDictionaryCode {
                column_id,
                row,
                byte_offset,
                code,
                dictionary_cardinality,
            },
        ) => {
            expected_column == column_id
                && expected_row == row
                && expected_offset == byte_offset
                && expected_code == code
                && expected_len == dictionary_cardinality
        }
        (
            metadata_adapter::MetadataProvenanceExpected::ColumnsRawStringLength {
                column_id: expected_column,
                row: expected_row,
                byte_offset: expected_offset,
                declared_bytes: expected_declared,
                available_bytes: expected_available,
            },
            MetadataDecodeProvenance::ColumnsRawStringLength {
                column_id,
                row,
                byte_offset,
                declared_bytes,
                available_bytes,
            },
        ) => {
            expected_column == column_id
                && expected_row == row
                && expected_offset == byte_offset
                && expected_declared == declared_bytes
                && expected_available == available_bytes
        }
        (
            metadata_adapter::MetadataProvenanceExpected::AliveBitmapTruncation {
                row_count: expected_rows,
                byte_offset: expected_offset,
                declared_bytes: expected_declared,
                observed_bytes: expected_observed,
            },
            MetadataDecodeProvenance::AliveBitmapTruncation {
                row_count,
                byte_offset,
                declared_bytes,
                observed_bytes,
            },
        ) => {
            expected_rows == row_count
                && expected_offset == byte_offset
                && expected_declared == declared_bytes
                && expected_observed == observed_bytes
        }
        _ => false,
    }
}

fn metadata_branch_dto(
    branch: SegmentBranch,
) -> Result<metadata_oracle::ExecutionBranchDto, String> {
    match branch {
        SegmentBranch::Pruned => Ok(metadata_oracle::ExecutionBranchDto::Pruned),
        SegmentBranch::ExactAllowList => Ok(metadata_oracle::ExecutionBranchDto::ExactAllowList),
        SegmentBranch::MaskedScan => Ok(metadata_oracle::ExecutionBranchDto::MaskedScan),
        SegmentBranch::FilteredGraph => Ok(metadata_oracle::ExecutionBranchDto::FilteredGraph),
        SegmentBranch::GraphExactFallback => {
            Ok(metadata_oracle::ExecutionBranchDto::GraphExactFallback)
        }
        SegmentBranch::Graph => {
            Err("unfiltered Graph branch cannot satisfy a metadata feature receipt".to_owned())
        }
    }
}

fn metadata_fallback_dto(fallback: PlanFallback) -> metadata_oracle::FallbackReasonDto {
    match fallback {
        PlanFallback::None => metadata_oracle::FallbackReasonDto::None,
        PlanFallback::VisitedBudget => metadata_oracle::FallbackReasonDto::VisitedBudget,
        PlanFallback::EfWidened => metadata_oracle::FallbackReasonDto::EfWidened,
        PlanFallback::CandidateShortfall => metadata_oracle::FallbackReasonDto::CandidateShortfall,
    }
}

fn validate_metadata_origin(
    query_id: u64,
    operation: &str,
    fault: &str,
    site: &str,
    cardinality: u64,
    effect: &str,
    receipt: &MetadataFeatureReceipt,
) -> Result<(), String> {
    let origin = &receipt.origin;
    let matches = receipt.query_id == query_id
        && origin.campaign() == "metadata-filter-planner"
        && origin.operation() == operation
        && origin.fault() == fault
        && origin.site() == site
        && u64::from(origin.cardinality()) == cardinality
        && origin.effect() == effect;
    if matches {
        Ok(())
    } else {
        Err(format!(
            "metadata feature receipt origin mismatch: expected query={query_id} campaign=metadata-filter-planner operation={operation} fault={fault} site={site} cardinality={cardinality} effect={effect:?}, observed query={} campaign={} operation={} fault={} site={} cardinality={} effect={:?}",
            receipt.query_id,
            origin.campaign(),
            origin.operation(),
            origin.fault(),
            origin.site(),
            origin.cardinality(),
            origin.effect(),
        ))
    }
}

fn validate_metadata_feature_receipts(
    expected: &[metadata_adapter::MetadataFeatureExpected],
    observed: &[MetadataFeatureReceipt],
    control: &metadata_adapter::MetadataControlEvidence,
) -> Result<(), String> {
    if expected.len() != observed.len() {
        return Err(format!(
            "metadata feature receipt cardinality mismatch: expected {}, observed {}",
            expected.len(),
            observed.len()
        ));
    }
    let observed_fault_results = u64::try_from(control.fault_results.len())
        .map_err(|_| "metadata fault result count exceeds u64".to_owned())?;
    for (index, (expected, observed)) in expected.iter().zip(observed).enumerate() {
        let detail_matches = match (expected, &observed.detail) {
            (
                metadata_adapter::MetadataFeatureExpected::ColumnDecodeRefused {
                    query_id,
                    source,
                    operation,
                    fault,
                    site,
                    cardinality,
                    field_class,
                    byte_offset,
                    error_class,
                    effect,
                    provenance,
                    expected_results,
                },
                MetadataFeatureDetail::ColumnDecodeRefused {
                    source: observed_source,
                    field_class: observed_class,
                    byte_offset: observed_offset,
                    error_class: observed_error,
                    provenance: observed_provenance,
                },
            ) => {
                validate_metadata_origin(
                    *query_id,
                    operation,
                    fault,
                    site,
                    *cardinality,
                    effect,
                    observed,
                )?;
                source == &metadata_source_label(*observed_source)
                    && field_class == observed_class
                    && byte_offset == observed_offset
                    && error_class == observed_error
                    && metadata_provenance_matches(provenance, observed_provenance)
                    && *expected_results == observed_fault_results
            }
            (
                metadata_adapter::MetadataFeatureExpected::AliveBitmapTruncationRefused {
                    query_id,
                    source,
                    operation,
                    fault,
                    site,
                    cardinality,
                    declared_rows,
                    byte_offset,
                    declared_bytes,
                    observed_bytes,
                    error_class,
                    effect,
                    provenance,
                    expected_results,
                },
                MetadataFeatureDetail::AliveBitmapTruncationRefused {
                    source: observed_source,
                    declared_rows: observed_rows,
                    declared_bytes: observed_declared,
                    observed_bytes: actual_observed,
                    byte_offset: observed_offset,
                    error_class: observed_error,
                    provenance: observed_provenance,
                },
            ) => {
                validate_metadata_origin(
                    *query_id,
                    operation,
                    fault,
                    site,
                    *cardinality,
                    effect,
                    observed,
                )?;
                source == &metadata_source_label(*observed_source)
                    && declared_rows == observed_rows
                    && byte_offset == observed_offset
                    && declared_bytes == observed_declared
                    && observed_bytes == actual_observed
                    && error_class == observed_error
                    && metadata_provenance_matches(provenance, observed_provenance)
                    && *expected_results == observed_fault_results
            }
            (
                metadata_adapter::MetadataFeatureExpected::SelectivityBoundaryChosen {
                    query_id,
                    source,
                    operation,
                    fault,
                    site,
                    cardinality,
                    filter_cardinality,
                    threshold,
                    branch,
                    effect,
                },
                MetadataFeatureDetail::SelectivityBoundaryChosen {
                    source: observed_source,
                    cardinality: observed_cardinality,
                    threshold: observed_threshold,
                    branch: observed_branch,
                },
            ) => {
                validate_metadata_origin(
                    *query_id,
                    operation,
                    fault,
                    site,
                    *cardinality,
                    effect,
                    observed,
                )?;
                source == &metadata_source_label(*observed_source)
                    && filter_cardinality == observed_cardinality
                    && threshold == observed_threshold
                    && *branch == metadata_branch_dto(*observed_branch)?
            }
            (
                metadata_adapter::MetadataFeatureExpected::VisitedBudgetFallback {
                    query_id,
                    source,
                    operation,
                    fault,
                    site,
                    cardinality,
                    visited,
                    budget,
                    filter_cardinality,
                    exact_rows_examined,
                    returned,
                    reason,
                    effect,
                },
                MetadataFeatureDetail::VisitedBudgetFallback {
                    source: observed_source,
                    visited: observed_visited,
                    budget: observed_budget,
                    filter_cardinality: observed_filter,
                    exact_rows_examined: observed_exact,
                    returned: observed_returned,
                    reason: observed_reason,
                },
            ) => {
                validate_metadata_origin(
                    *query_id,
                    operation,
                    fault,
                    site,
                    *cardinality,
                    effect,
                    observed,
                )?;
                source == &metadata_source_label(*observed_source)
                    && *visited
                        == u64::try_from(*observed_visited)
                            .map_err(|_| "metadata visited count exceeds u64".to_owned())?
                    && *budget
                        == u64::try_from(*observed_budget)
                            .map_err(|_| "metadata visited budget exceeds u64".to_owned())?
                    && filter_cardinality == observed_filter
                    && exact_rows_examined == observed_exact
                    && returned == observed_returned
                    && *reason == metadata_fallback_dto(*observed_reason)
            }
            _ => false,
        };
        if !detail_matches {
            return Err(format!(
                "metadata feature receipt {index} guard/effect mismatch: expected {expected:?}, observed {observed:?}"
            ));
        }
    }
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "one metadata comparison owns all exact replay evidence streams"
)]
fn record_metadata_predicate_coverage(
    predicate: &metadata_oracle::PredicateDto,
    coverage: &mut CoverageRegistry,
) {
    match predicate {
        metadata_oracle::PredicateDto::Eq { .. } => coverage.hit("metadata.predicate.eq"),
        metadata_oracle::PredicateDto::In { .. } => coverage.hit("metadata.predicate.in"),
        metadata_oracle::PredicateDto::Range { .. } => coverage.hit("metadata.predicate.range"),
        metadata_oracle::PredicateDto::Exists(_) => coverage.hit("metadata.predicate.exists"),
        metadata_oracle::PredicateDto::IsNull(_) => coverage.hit("metadata.predicate.is-null"),
        metadata_oracle::PredicateDto::And(children) => {
            coverage.hit("metadata.predicate.and");
            for child in children {
                record_metadata_predicate_coverage(child, coverage);
            }
        }
        metadata_oracle::PredicateDto::Or(children) => {
            coverage.hit("metadata.predicate.or");
            for child in children {
                record_metadata_predicate_coverage(child, coverage);
            }
        }
        metadata_oracle::PredicateDto::Not(child) => {
            coverage.hit("metadata.predicate.not");
            record_metadata_predicate_coverage(child, coverage);
        }
    }
}

fn record_metadata_i39_coverage(
    receipts: &[MetadataExecutionReceipt],
    coverage: &mut CoverageRegistry,
) {
    for receipt in receipts {
        let branch = match receipt.branch {
            SegmentBranch::Pruned => "pruned",
            SegmentBranch::ExactAllowList => "exact-allow-list",
            SegmentBranch::MaskedScan => "masked-scan",
            SegmentBranch::FilteredGraph => "filtered-graph",
            SegmentBranch::GraphExactFallback => "graph-exact-fallback",
            SegmentBranch::Graph => continue,
        };
        coverage.hit(format!("metadata.i39.branch.{branch}"));
        let fallback = match receipt.fallback {
            PlanFallback::None => Some("none"),
            PlanFallback::VisitedBudget => Some("visited-budget"),
            PlanFallback::CandidateShortfall => Some("candidate-shortfall"),
            PlanFallback::EfWidened => None,
        };
        if let Some(fallback) = fallback {
            coverage.hit(format!("metadata.i39.fallback.{fallback}"));
        }
    }
}

fn record_metadata_family_coverage(
    evidence: &metadata_adapter::MetadataOperationEvidence,
    coverage: &mut CoverageRegistry,
) -> Result<(), String> {
    if evidence.control.clean_initial_directory != evidence.control.fault_initial_directory {
        return Err("metadata same-seed fixture directories are not byte-identical".to_owned());
    }
    coverage.hit("metadata.control.byte-identical");
    for query in &evidence.queries {
        record_metadata_predicate_coverage(&query.predicate, coverage);
    }

    let mut column_receipts = 0_usize;
    let mut bitmap_receipts = 0_usize;
    let mut selectivity_receipts = 0_usize;
    let mut visited_receipts = 0_usize;
    for receipt in &evidence.feature_receipts {
        match &receipt.detail {
            MetadataFeatureDetail::ColumnDecodeRefused { provenance, .. } => {
                column_receipts = column_receipts.saturating_add(1);
                let class = match provenance {
                    zeppelin_embed::segment::MetadataDecodeProvenance::ColumnsPresenceTail {
                        ..
                    } => "presence-tail",
                    zeppelin_embed::segment::MetadataDecodeProvenance::ColumnsDictionaryCode {
                        ..
                    } => "dictionary-code",
                    zeppelin_embed::segment::MetadataDecodeProvenance::ColumnsRawStringLength {
                        ..
                    } => "raw-string-length",
                    zeppelin_embed::segment::MetadataDecodeProvenance::AliveBitmapTruncation {
                        ..
                    } => {
                        return Err(
                            "metadata column refusal used Alive bitmap provenance".to_owned()
                        );
                    }
                };
                coverage.hit(format!("metadata.mutation.columns.{class}"));
            }
            MetadataFeatureDetail::AliveBitmapTruncationRefused { .. } => {
                bitmap_receipts = bitmap_receipts.saturating_add(1);
            }
            MetadataFeatureDetail::SelectivityBoundaryChosen { .. } => {
                selectivity_receipts = selectivity_receipts.saturating_add(1);
            }
            MetadataFeatureDetail::VisitedBudgetFallback { .. } => {
                visited_receipts = visited_receipts.saturating_add(1);
            }
        }
    }
    if column_receipts == 1 {
        coverage.hit("metadata.receipt.column-corruption.cardinality-one");
    }
    if bitmap_receipts == 1 {
        coverage.hit("metadata.receipt.bitmap-truncation.cardinality-one");
    }
    if selectivity_receipts == 2 {
        coverage.hit("metadata.receipt.selectivity-boundary.cardinality-two");
    }
    if visited_receipts == 1 {
        coverage.hit("metadata.receipt.visited-budget.cardinality-one");
    }

    match &evidence.invariant {
        metadata_adapter::MetadataInvariantEvidence::I38 { input, observed } => {
            match &input.predicate {
                metadata_oracle::PredicateDto::Eq { .. } => {
                    coverage.hit("metadata.i38.predicate.eq");
                }
                metadata_oracle::PredicateDto::Range {
                    lower: Some(lower), ..
                } if lower.inclusive => {
                    coverage.hit("metadata.i38.predicate.range-inclusive");
                }
                metadata_oracle::PredicateDto::Range { lower: Some(_), .. } => {
                    coverage.hit("metadata.i38.predicate.range-exclusive-lower");
                }
                _ => {}
            }
            if input.sources.iter().any(|source| !source.sealed) {
                coverage.hit("metadata.i38.source.active");
            }
            if input.sources.iter().filter(|source| source.sealed).count() >= 3 {
                coverage.hit("metadata.i38.source.sealed-three-plus");
            }
            if input.sources.iter().any(|source| {
                source.sealed && source.range == metadata_oracle::SourceRangeDto::Empty
            }) {
                coverage.hit("metadata.i38.source.empty");
            }
            if input.sources.iter().any(|source| {
                source.sealed && source.range == metadata_oracle::SourceRangeDto::Unstamped
            }) {
                coverage.hit("metadata.i38.source.missing-bounds");
            }
            if input.expected_delete_records > 0
                && input.expected_delete_records == observed.wal_delete_records
            {
                coverage.hit("metadata.i38.source.public-delete-wal");
            }
            let sources = input
                .rows
                .iter()
                .map(|row| row.source.as_str())
                .collect::<BTreeSet<_>>();
            if sources.iter().any(|source| {
                source.starts_with("sealed-")
                    && !input
                        .live
                        .iter()
                        .any(|(live_source, _)| live_source == source)
            }) {
                coverage.hit("metadata.i38.source.all-tombstoned");
            }
        }
        metadata_adapter::MetadataInvariantEvidence::I39 { .. } => {
            record_metadata_i39_coverage(&evidence.execution_receipts, coverage);
        }
        metadata_adapter::MetadataInvariantEvidence::I37 { .. } => {
            coverage.hit(format!(
                "metadata.i37.matrix.{}",
                metadata_adapter::i37_predicate_case_key(evidence.control.seed)
            ));
        }
        metadata_adapter::MetadataInvariantEvidence::I36 { .. } => {}
    }
    Ok(())
}

fn metadata_region_key(region: zeppelin_embed::segment::layout::RegionKind) -> &'static str {
    use zeppelin_embed::segment::layout::RegionKind;
    match region {
        RegionKind::Columns => "columns",
        RegionKind::Alive => "alive",
        RegionKind::VectorCodes => "vector-codes",
        RegionKind::VectorFactors => "vector-factors",
        RegionKind::VectorRescore => "vector-rescore",
        RegionKind::Postings => "postings",
        RegionKind::GraphNodeBlocks => "graph-node-blocks",
        RegionKind::GraphColocatedCodes => "graph-colocated-codes",
        RegionKind::SignPlane => "sign-plane",
        RegionKind::PdxClusteredBlocks => "pdx-clustered-blocks",
        RegionKind::ChecksumTable => "checksum-table",
        RegionKind::DocumentVersions => "document-versions",
        RegionKind::StoredMetadata => "stored-metadata",
        RegionKind::StoredText => "stored-text",
        RegionKind::VectorSpaceN => "vector-space-n",
    }
}

fn metadata_checksum_field_json(field: &metadata_adapter::MetadataChecksumField) -> String {
    match field {
        metadata_adapter::MetadataChecksumField::TargetRegion { region } => format!(
            "{{\"kind\":\"target-region\",\"region\":{{\"code\":{},\"name\":\"{}\"}}}}",
            region.id(),
            metadata_region_key(*region),
        ),
        metadata_adapter::MetadataChecksumField::TargetRegionChunk {
            region,
            chunk_index,
        } => format!(
            "{{\"kind\":\"target-region-chunk\",\"region\":{{\"code\":{},\"name\":\"{}\"}},\"chunk_index\":{chunk_index}}}",
            region.id(),
            metadata_region_key(*region),
        ),
        metadata_adapter::MetadataChecksumField::ChecksumTableRegion => {
            "{\"kind\":\"checksum-table-region\"}".to_owned()
        }
        metadata_adapter::MetadataChecksumField::SegmentHeader => {
            "{\"kind\":\"segment-header\"}".to_owned()
        }
        metadata_adapter::MetadataChecksumField::SegmentWholeFile => {
            "{\"kind\":\"segment-whole-file\"}".to_owned()
        }
        metadata_adapter::MetadataChecksumField::GraphInternal => {
            "{\"kind\":\"graph-internal\"}".to_owned()
        }
    }
}

fn metadata_mutation_json(mutation: &metadata_adapter::MetadataMutationEvidence) -> String {
    let optional_byte =
        |value: Option<u8>| value.map_or_else(|| "null".to_owned(), |value| value.to_string());
    let checksum_rewrites = mutation
        .checksum_rewrites
        .iter()
        .map(|rewrite| {
            format!(
                "{{\"field\":{},\"absolute_offset\":{},\"before\":{},\"after\":{}}}",
                metadata_checksum_field_json(&rewrite.field),
                rewrite.absolute_offset,
                rewrite.before,
                rewrite.after,
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{{\"source\":\"{}\",\"region\":{{\"code\":{},\"name\":\"{}\"}},\"region_offset\":{},\"field_offset\":{},\"absolute_offset\":{},\"before_hex\":\"{}\",\"after_hex\":\"{}\",\"left_neighbor_before\":{},\"left_neighbor_after\":{},\"right_neighbor_before\":{},\"right_neighbor_after\":{},\"declared_bytes_before\":{},\"declared_bytes_after\":{},\"observed_bytes_after\":{},\"checksum_rewrites\":[{checksum_rewrites}],\"post_mutation_artifact_digest\":{}}}",
        json_escape(&mutation.source),
        mutation.region.id(),
        metadata_region_key(mutation.region),
        mutation.region_offset,
        mutation.field_offset,
        mutation.absolute_offset,
        evidence_hex(&mutation.before),
        evidence_hex(&mutation.after),
        optional_byte(mutation.left_neighbor_before),
        optional_byte(mutation.left_neighbor_after),
        optional_byte(mutation.right_neighbor_before),
        optional_byte(mutation.right_neighbor_after),
        mutation.declared_bytes_before,
        mutation.declared_bytes_after,
        mutation.observed_bytes_after,
        mutation.post_mutation_artifact_digest,
    )
}

fn metadata_scalar_json(value: &metadata_oracle::ScalarCell) -> String {
    match value {
        metadata_oracle::ScalarCell::Null => "{\"kind\":\"null\"}".to_owned(),
        metadata_oracle::ScalarCell::U64(value) => {
            format!("{{\"kind\":\"u64\",\"value\":{value}}}")
        }
        metadata_oracle::ScalarCell::I64(value) => {
            format!("{{\"kind\":\"i64\",\"value\":{value}}}")
        }
        metadata_oracle::ScalarCell::F64Bits(value) => {
            format!("{{\"kind\":\"f64-bits\",\"value\":{value}}}")
        }
        metadata_oracle::ScalarCell::Bool(value) => {
            format!("{{\"kind\":\"bool\",\"value\":{value}}}")
        }
        metadata_oracle::ScalarCell::Utf8(value) => format!(
            "{{\"kind\":\"utf8\",\"bytes_hex\":\"{}\"}}",
            evidence_hex(value)
        ),
    }
}

fn metadata_cells_json(cells: &BTreeMap<u32, metadata_oracle::ScalarCell>) -> String {
    cells
        .iter()
        .map(|(column, value)| {
            format!(
                "{{\"column\":{column},\"value\":{}}}",
                metadata_scalar_json(value)
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn metadata_column_kind_key(kind: metadata_oracle::ColumnKind) -> &'static str {
    match kind {
        metadata_oracle::ColumnKind::U64 => "u64",
        metadata_oracle::ColumnKind::I64 => "i64",
        metadata_oracle::ColumnKind::F64 => "f64",
        metadata_oracle::ColumnKind::Bool => "bool",
        metadata_oracle::ColumnKind::DictionaryString => "dictionary-string",
        metadata_oracle::ColumnKind::RawString => "raw-string",
    }
}

fn metadata_i36_input_json(input: &metadata_oracle::I36Input) -> String {
    let definitions = input
        .definitions
        .iter()
        .map(|definition| {
            format!(
                "{{\"id\":{},\"name_hex\":\"{}\",\"kind\":\"{}\",\"nullable\":{}}}",
                definition.id,
                evidence_hex(&definition.name),
                metadata_column_kind_key(definition.kind),
                definition.nullable,
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let rows = input
        .rows
        .iter()
        .map(|cells| format!("[{}]", metadata_cells_json(cells)))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{{\"source\":\"{}\",\"definitions\":[{definitions}],\"rows\":[{rows}]}}",
        json_escape(&input.source),
    )
}

fn metadata_predicate_json(predicate: &metadata_oracle::PredicateDto) -> String {
    match predicate {
        metadata_oracle::PredicateDto::Eq { column, value } => format!(
            "{{\"kind\":\"eq\",\"column\":{column},\"value\":{}}}",
            metadata_scalar_json(value)
        ),
        metadata_oracle::PredicateDto::In { column, values } => {
            let values = values
                .iter()
                .map(metadata_scalar_json)
                .collect::<Vec<_>>()
                .join(",");
            format!("{{\"kind\":\"in\",\"column\":{column},\"values\":[{values}]}}")
        }
        metadata_oracle::PredicateDto::Range {
            column,
            lower,
            upper,
        } => {
            let bound = |bound: &Option<metadata_oracle::RangeBoundDto>| {
                bound.as_ref().map_or_else(
                    || "null".to_owned(),
                    |bound| {
                        format!(
                            "{{\"value\":{},\"inclusive\":{}}}",
                            metadata_scalar_json(&bound.value),
                            bound.inclusive,
                        )
                    },
                )
            };
            format!(
                "{{\"kind\":\"range\",\"column\":{column},\"lower\":{},\"upper\":{}}}",
                bound(lower),
                bound(upper),
            )
        }
        metadata_oracle::PredicateDto::Exists(column) => {
            format!("{{\"kind\":\"exists\",\"column\":{column}}}")
        }
        metadata_oracle::PredicateDto::IsNull(column) => {
            format!("{{\"kind\":\"is-null\",\"column\":{column}}}")
        }
        metadata_oracle::PredicateDto::And(children) => {
            let children = children
                .iter()
                .map(metadata_predicate_json)
                .collect::<Vec<_>>()
                .join(",");
            format!("{{\"kind\":\"and\",\"children\":[{children}]}}")
        }
        metadata_oracle::PredicateDto::Or(children) => {
            let children = children
                .iter()
                .map(metadata_predicate_json)
                .collect::<Vec<_>>()
                .join(",");
            format!("{{\"kind\":\"or\",\"children\":[{children}]}}")
        }
        metadata_oracle::PredicateDto::Not(child) => format!(
            "{{\"kind\":\"not\",\"child\":{}}}",
            metadata_predicate_json(child)
        ),
    }
}

fn metadata_row_json(row: &metadata_oracle::MetadataRowDto) -> String {
    format!(
        "{{\"row_id\":{},\"cells\":[{}]}}",
        row.row_id,
        metadata_cells_json(&row.cells),
    )
}

fn metadata_i37_input_json(input: &metadata_oracle::I37Input) -> String {
    let rows = input
        .rows
        .iter()
        .map(metadata_row_json)
        .collect::<Vec<_>>()
        .join(",");
    let live = input
        .live
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let sources = input
        .sources
        .iter()
        .map(|source| {
            let rows = source
                .rows
                .iter()
                .map(metadata_row_json)
                .collect::<Vec<_>>()
                .join(",");
            let live = source
                .live
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(",");
            format!(
                "{{\"source\":\"{}\",\"sealed\":{},\"rows\":[{rows}],\"live\":[{live}]}}",
                json_escape(&source.source),
                source.sealed,
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{{\"rows\":[{rows}],\"live\":[{live}],\"predicate\":{},\"sources\":[{sources}]}}",
        metadata_predicate_json(&input.predicate),
    )
}

fn metadata_source_range_json(range: metadata_oracle::SourceRangeDto) -> String {
    match range {
        metadata_oracle::SourceRangeDto::Unstamped => "{\"kind\":\"unstamped\"}".to_owned(),
        metadata_oracle::SourceRangeDto::Empty => "{\"kind\":\"empty\"}".to_owned(),
        metadata_oracle::SourceRangeDto::Bounded { min, max } => {
            format!("{{\"kind\":\"bounded\",\"min\":{min},\"max\":{max}}}")
        }
    }
}

fn metadata_exact_hit_json(hit: &metadata_oracle::ExactHitDto) -> String {
    format!(
        "{{\"source\":\"{}\",\"row_id\":{},\"document_id\":\"{}\",\"distance_bits\":{}}}",
        json_escape(&hit.source),
        hit.row_id,
        hit.document_id,
        hit.distance_bits,
    )
}

fn metadata_i38_input_json(input: &metadata_oracle::I38Input) -> String {
    let sources = input
        .sources
        .iter()
        .map(|source| {
            format!(
                "{{\"source\":\"{}\",\"sealed\":{},\"range\":{}}}",
                json_escape(&source.source),
                source.sealed,
                metadata_source_range_json(source.range),
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let rows = input
        .rows
        .iter()
        .map(|row| {
            format!(
                "{{\"source\":\"{}\",\"row_id\":{},\"document_id\":\"{}\",\"cells\":[{}]}}",
                json_escape(&row.source),
                row.row_id,
                row.document_id,
                metadata_cells_json(&row.cells),
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let live = input
        .live
        .iter()
        .map(|(source, row)| {
            format!(
                "{{\"source\":\"{}\",\"row_id\":{row}}}",
                json_escape(source)
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let hits = input
        .unfiltered_exact
        .iter()
        .map(metadata_exact_hit_json)
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{{\"sources\":[{sources}],\"rows\":[{rows}],\"live\":[{live}],\"predicate\":{},\"unfiltered_exact\":[{hits}],\"expected_delete_records\":{}}}",
        metadata_predicate_json(&input.predicate),
        input.expected_delete_records,
    )
}

fn metadata_fallback_key(reason: metadata_oracle::FallbackReasonDto) -> &'static str {
    match reason {
        metadata_oracle::FallbackReasonDto::None => "none",
        metadata_oracle::FallbackReasonDto::VisitedBudget => "visited-budget",
        metadata_oracle::FallbackReasonDto::CandidateShortfall => "candidate-shortfall",
        metadata_oracle::FallbackReasonDto::EfWidened => "ef-widened",
    }
}

fn metadata_i39_case_json(case: &metadata_oracle::I39ExpectedCase) -> String {
    let mode = match case.mode {
        metadata_oracle::I39ExecutionModeDto::ExactScan { source_may_match } => {
            format!("{{\"kind\":\"exact-scan\",\"source_may_match\":{source_may_match}}}")
        }
        metadata_oracle::I39ExecutionModeDto::FilteredGraph { required_fallback } => format!(
            "{{\"kind\":\"filtered-graph\",\"required_fallback\":\"{}\"}}",
            metadata_fallback_key(required_fallback),
        ),
    };
    let optional =
        |value: Option<u64>| value.map_or_else(|| "null".to_owned(), |value| value.to_string());
    format!(
        "{{\"key\":{{\"query_id\":{},\"source\":\"{}\"}},\"mode\":{mode},\"row_count\":{},\"filter_cardinality\":{},\"allow_list_threshold\":{},\"rows_examined\":{},\"allowed_rows_examined\":{},\"vectors_scored\":{},\"graph_nodes_visited\":{},\"exact_fallback_rows_examined\":{},\"returned_candidates\":{},\"ef_effective\":{},\"visited_budget\":{},\"sealed\":{}}}",
        case.key.query_id,
        json_escape(&case.key.source),
        case.row_count,
        case.filter_cardinality,
        case.allow_list_threshold,
        case.rows_examined,
        case.allowed_rows_examined,
        case.vectors_scored,
        case.graph_nodes_visited,
        case.exact_fallback_rows_examined,
        case.returned_candidates,
        optional(case.ef_effective),
        optional(case.visited_budget),
        case.sealed,
    )
}

fn metadata_fixture_json(fixture: &metadata_adapter::MetadataFixtureEvidence) -> String {
    match fixture {
        metadata_adapter::MetadataFixtureEvidence::Columns(input) => format!(
            "{{\"kind\":\"columns\",\"payload\":{}}}",
            metadata_i36_input_json(input)
        ),
        metadata_adapter::MetadataFixtureEvidence::Bitmap(input) => format!(
            "{{\"kind\":\"bitmap\",\"payload\":{}}}",
            metadata_i37_input_json(input)
        ),
        metadata_adapter::MetadataFixtureEvidence::Planner(input) => format!(
            "{{\"kind\":\"planner\",\"payload\":{}}}",
            metadata_i38_input_json(input)
        ),
        metadata_adapter::MetadataFixtureEvidence::Execution(cases) => {
            let cases = cases
                .iter()
                .map(metadata_i39_case_json)
                .collect::<Vec<_>>()
                .join(",");
            format!("{{\"kind\":\"execution\",\"payload\":[{cases}]}}")
        }
    }
}

fn metadata_span_json(span: metadata_oracle::ByteSpan) -> String {
    format!("{{\"start\":{},\"end\":{}}}", span.start, span.end)
}

fn metadata_string_spans_json(spans: metadata_oracle::StringByteSpans) -> String {
    format!(
        "{{\"full\":{},\"length\":{},\"payload\":{}}}",
        metadata_span_json(spans.full),
        metadata_span_json(spans.length),
        metadata_span_json(spans.payload),
    )
}

fn metadata_definition_json(definition: &metadata_oracle::ColumnDefinitionDto) -> String {
    format!(
        "{{\"id\":{},\"name_hex\":\"{}\",\"kind\":\"{}\",\"nullable\":{}}}",
        definition.id,
        evidence_hex(&definition.name),
        metadata_column_kind_key(definition.kind),
        definition.nullable,
    )
}

fn metadata_physical_cell_json(cell: &metadata_oracle::PhysicalCell) -> String {
    match cell {
        metadata_oracle::PhysicalCell::U64(value) => {
            format!("{{\"kind\":\"u64\",\"value\":{value}}}")
        }
        metadata_oracle::PhysicalCell::I64(value) => {
            format!("{{\"kind\":\"i64\",\"value\":{value}}}")
        }
        metadata_oracle::PhysicalCell::F64Bits(value) => {
            format!("{{\"kind\":\"f64-bits\",\"value\":{value}}}")
        }
        metadata_oracle::PhysicalCell::BoolByte(value) => {
            format!("{{\"kind\":\"bool-byte\",\"value\":{value}}}")
        }
        metadata_oracle::PhysicalCell::DictionaryCode { code, decoded } => {
            let decoded = decoded.as_ref().map_or_else(
                || "null".to_owned(),
                |decoded| format!("\"{}\"", evidence_hex(decoded)),
            );
            format!("{{\"kind\":\"dictionary-code\",\"code\":{code},\"decoded_hex\":{decoded}}}")
        }
        metadata_oracle::PhysicalCell::RawUtf8(value) => format!(
            "{{\"kind\":\"raw-utf8\",\"bytes_hex\":\"{}\"}}",
            evidence_hex(value)
        ),
    }
}

fn metadata_parsed_columns_json(columns: &metadata_oracle::ParsedColumns) -> String {
    let definitions = columns
        .definitions
        .iter()
        .map(metadata_definition_json)
        .collect::<Vec<_>>()
        .join(",");
    let definition_spans = columns
        .definition_spans
        .iter()
        .map(|(column, spans)| {
            format!(
                "{{\"column\":{column},\"full\":{},\"id\":{},\"kind\":{},\"nullable\":{},\"name\":{}}}",
                metadata_span_json(spans.full),
                metadata_span_json(spans.id),
                metadata_span_json(spans.kind),
                metadata_span_json(spans.nullable),
                metadata_string_spans_json(spans.name),
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let presence_spans = columns
        .presence_spans
        .iter()
        .map(|(column, spans)| {
            format!(
                "{{\"column\":{column},\"length\":{},\"bitmap\":{}}}",
                metadata_span_json(spans.length),
                metadata_span_json(spans.bitmap),
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let dictionary_spans = columns
        .dictionary_spans
        .iter()
        .map(|(column, spans)| {
            let entries = spans
                .entries
                .iter()
                .map(|entry| metadata_string_spans_json(*entry))
                .collect::<Vec<_>>()
                .join(",");
            format!(
                "{{\"column\":{column},\"count\":{},\"entries\":[{entries}],\"width\":{},\"reserved\":{}}}",
                metadata_span_json(spans.count),
                metadata_span_json(spans.width),
                metadata_span_json(spans.reserved),
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let cells = columns
        .cells
        .iter()
        .map(|((row, column), cell)| {
            let length_span = cell.length_span.map_or_else(
                || "null".to_owned(),
                metadata_span_json,
            );
            format!(
                "{{\"row\":{row},\"column\":{column},\"present\":{},\"logical\":{},\"physical\":{},\"span\":{},\"length_span\":{length_span},\"payload_span\":{}}}",
                cell.present,
                metadata_scalar_json(&cell.logical),
                metadata_physical_cell_json(&cell.physical),
                metadata_span_json(cell.span),
                metadata_span_json(cell.payload_span),
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{{\"row_count\":{},\"definitions\":[{definitions}],\"row_count_span\":{},\"column_count_span\":{},\"definition_spans\":[{definition_spans}],\"presence_spans\":[{presence_spans}],\"dictionary_spans\":[{dictionary_spans}],\"cells\":[{cells}]}}",
        columns.row_count,
        metadata_span_json(columns.row_count_span),
        metadata_span_json(columns.column_count_span),
    )
}

fn metadata_logical_rows_json(rows: &[BTreeMap<u32, metadata_oracle::ScalarCell>]) -> String {
    rows.iter()
        .map(|cells| format!("[{}]", metadata_cells_json(cells)))
        .collect::<Vec<_>>()
        .join(",")
}

fn metadata_i36_observed_json(observed: &metadata_oracle::I36Observed) -> String {
    format!(
        "{{\"active_rows\":[{}],\"raw\":{},\"reader_rows\":[{}],\"public_rows\":[{}]}}",
        metadata_logical_rows_json(&observed.active_rows),
        metadata_parsed_columns_json(&observed.raw),
        metadata_logical_rows_json(&observed.reader_rows),
        metadata_logical_rows_json(&observed.public_rows),
    )
}

fn metadata_branch_key(branch: metadata_oracle::ExecutionBranchDto) -> &'static str {
    match branch {
        metadata_oracle::ExecutionBranchDto::Pruned => "pruned",
        metadata_oracle::ExecutionBranchDto::ExactAllowList => "exact-allow-list",
        metadata_oracle::ExecutionBranchDto::MaskedScan => "masked-scan",
        metadata_oracle::ExecutionBranchDto::FilteredGraph => "filtered-graph",
        metadata_oracle::ExecutionBranchDto::GraphExactFallback => "graph-exact-fallback",
    }
}

fn metadata_branch_report_json(report: &metadata_oracle::BranchReportDto) -> String {
    format!(
        "{{\"key\":{{\"query_id\":{},\"source\":\"{}\"}},\"branch\":\"{}\",\"fallback\":\"{}\",\"filter_cardinality\":{}}}",
        report.key.query_id,
        json_escape(&report.key.source),
        metadata_branch_key(report.branch),
        metadata_fallback_key(report.fallback),
        report.filter_cardinality,
    )
}

fn metadata_execution_receipt_json(receipt: &metadata_oracle::ExecutionReceiptDto) -> String {
    let optional =
        |value: Option<u64>| value.map_or_else(|| "null".to_owned(), |value| value.to_string());
    format!(
        "{{\"key\":{{\"query_id\":{},\"source\":\"{}\"}},\"branch\":\"{}\",\"fallback\":\"{}\",\"row_count\":{},\"filter_cardinality\":{},\"rows_examined\":{},\"allowed_rows_examined\":{},\"vectors_scored\":{},\"graph_nodes_visited\":{},\"exact_fallback_rows_examined\":{},\"returned_candidates\":{},\"ef_effective\":{},\"visited_budget\":{},\"sealed\":{}}}",
        receipt.key.query_id,
        json_escape(&receipt.key.source),
        metadata_branch_key(receipt.branch),
        metadata_fallback_key(receipt.fallback),
        receipt.row_count,
        receipt.filter_cardinality,
        receipt.rows_examined,
        receipt.allowed_rows_examined,
        receipt.vectors_scored,
        receipt.graph_nodes_visited,
        receipt.exact_fallback_rows_examined,
        receipt.returned_candidates,
        optional(receipt.ef_effective),
        optional(receipt.visited_budget),
        receipt.sealed,
    )
}

fn metadata_i37_observed_json(observed: &metadata_oracle::I37Observed) -> String {
    let set = |values: &BTreeSet<u32>| {
        values
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",")
    };
    let sources = observed
        .sources
        .iter()
        .map(|source| {
            format!(
                "{{\"source\":\"{}\",\"sealed\":{},\"row_count\":{},\"live\":[{}],\"evaluator\":[{}],\"public_results\":[{}],\"report\":{},\"receipt\":{}}}",
                json_escape(&source.source),
                source.sealed,
                source.row_count,
                set(&source.live),
                set(&source.evaluator),
                source
                    .public_results
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
                metadata_branch_report_json(&source.report),
                metadata_execution_receipt_json(&source.receipt),
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{{\"evaluator\":[{}],\"public_results\":[{}],\"sources\":[{sources}],\"allow_list_threshold\":{}}}",
        set(&observed.evaluator),
        observed
            .public_results
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(","),
        observed.allow_list_threshold,
    )
}

fn metadata_i38_observed_json(observed: &metadata_oracle::I38Observed) -> String {
    let hits = observed
        .filtered_exact
        .iter()
        .map(metadata_exact_hit_json)
        .collect::<Vec<_>>()
        .join(",");
    let pruned = observed
        .pruned_sources
        .iter()
        .map(|source| format!("\"{}\"", json_escape(source)))
        .collect::<Vec<_>>()
        .join(",");
    let reports = observed
        .reports
        .iter()
        .map(metadata_branch_report_json)
        .collect::<Vec<_>>()
        .join(",");
    let receipts = observed
        .execution_receipts
        .iter()
        .map(metadata_execution_receipt_json)
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{{\"filtered_exact\":[{hits}],\"pruned_sources\":[{pruned}],\"reports\":[{reports}],\"execution_receipts\":[{receipts}],\"allow_list_threshold\":{},\"wal_delete_records\":{}}}",
        observed.allow_list_threshold, observed.wal_delete_records,
    )
}

fn metadata_i39_observed_json(observed: &metadata_oracle::I39Observed) -> String {
    let reports = |values: &[metadata_oracle::BranchReportDto]| {
        values
            .iter()
            .map(metadata_branch_report_json)
            .collect::<Vec<_>>()
            .join(",")
    };
    let receipts = observed
        .receipts
        .iter()
        .map(metadata_execution_receipt_json)
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{{\"reports\":[{}],\"diagnostics_reports\":[{}],\"receipts\":[{receipts}],\"allow_list_threshold\":{}}}",
        reports(&observed.reports),
        reports(&observed.diagnostics_reports),
        observed.allow_list_threshold,
    )
}

fn metadata_result_json(result: &metadata_adapter::MetadataResultFact) -> String {
    let document_id = result.document_id.map_or_else(
        || "null".to_owned(),
        |document_id| format!("\"{document_id}\""),
    );
    format!(
        "{{\"source\":\"{}\",\"row_id\":{},\"document_id\":{document_id},\"score_bits\":{}}}",
        json_escape(&result.source),
        result.row_id,
        result.score_bits,
    )
}

fn metadata_results_json(results: &[metadata_adapter::MetadataResultFact]) -> String {
    results
        .iter()
        .map(metadata_result_json)
        .collect::<Vec<_>>()
        .join(",")
}

fn metadata_directory_json(
    directory: &metadata_adapter::MetadataDirectoryDigestEvidence,
) -> String {
    let files = directory
        .files
        .iter()
        .map(|file| {
            format!(
                "{{\"relative_path\":\"{}\",\"byte_length\":{},\"digest\":{}}}",
                json_escape(&file.relative_path),
                file.byte_length,
                file.digest,
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("{{\"digest\":{},\"files\":[{files}]}}", directory.digest)
}

fn metadata_control_json(control: &metadata_adapter::MetadataControlEvidence) -> String {
    let directory_relation = match control.directory_relation {
        metadata_adapter::MetadataDirectoryRelation::UnpairedSingleDirectory => {
            "unpaired-single-directory"
        }
        metadata_adapter::MetadataDirectoryRelation::DistinctByteIdentical => {
            "distinct-byte-identical"
        }
    };
    let outcome = match control.outcome {
        metadata_adapter::MetadataControlOutcome::Only => "clean-only",
        metadata_adapter::MetadataControlOutcome::FilteredSucceeded => {
            "clean-and-filtered-succeeded"
        }
        metadata_adapter::MetadataControlOutcome::FaultAndRetryEquivalent => {
            "clean-fault-and-retry-equivalent"
        }
        metadata_adapter::MetadataControlOutcome::FaultRefusedRetryEquivalent => {
            "clean-succeeded-fault-refused-retry-equivalent"
        }
        metadata_adapter::MetadataControlOutcome::GraphFaultFallbackRetryEquivalent => {
            "clean-graph-fault-fallback-retry-equivalent"
        }
    };
    let fault_error = control.fault_error.as_ref().map_or_else(
        || "null".to_owned(),
        |error| format!("\"{}\"", json_escape(error)),
    );
    format!(
        "{{\"namespace\":\"{}\",\"seed\":{},\"query_id_base\":{},\"normalized_schedule_digest\":{},\"directory_relation\":\"{directory_relation}\",\"outcome\":\"{outcome}\",\"clean_results\":[{}],\"fault_results\":[{}],\"retry_results\":[{}],\"independent_expected_results\":[{}],\"fault_error\":{fault_error},\"clean_generation\":{},\"fault_generation\":{},\"retry_generation\":{},\"clean_wal_digest\":{},\"fault_wal_digest\":{},\"retry_wal_digest\":{},\"clean_source_digest\":{},\"fault_source_digest\":{},\"retry_source_digest\":{},\"clean_initial_directory\":{},\"fault_initial_directory\":{}}}",
        json_escape(control.namespace),
        control.seed,
        control.query_id_base,
        control.normalized_schedule_digest,
        metadata_results_json(&control.clean_results),
        metadata_results_json(&control.fault_results),
        metadata_results_json(&control.retry_results),
        metadata_results_json(&control.independent_expected_results),
        control.clean_generation,
        control.fault_generation,
        control.retry_generation,
        control.clean_wal_digest,
        control.fault_wal_digest,
        control.retry_wal_digest,
        control.clean_source_digest,
        control.fault_source_digest,
        control.retry_source_digest,
        metadata_directory_json(&control.clean_initial_directory),
        metadata_directory_json(&control.fault_initial_directory),
    )
}

fn metadata_adapter_fault_key(fault: metadata_adapter::MetadataFaultKind) -> &'static str {
    match fault {
        metadata_adapter::MetadataFaultKind::ColumnCorruption => "column-corruption",
        metadata_adapter::MetadataFaultKind::BitmapTruncation => "bitmap-truncation",
        metadata_adapter::MetadataFaultKind::SelectivityBoundary => "selectivity-boundary",
        metadata_adapter::MetadataFaultKind::VisitedBudgetFallback => "visited-budget-fallback",
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "one metadata comparison owns all exact replay evidence streams"
)]
fn record_metadata_evidence(
    operation: super::campaign::MetadataOperation,
    evidence: metadata_adapter::MetadataOperationEvidence,
    seed: u64,
    profile: FaultProfile,
    op_index: usize,
    oracle_records: &mut Vec<OracleRecord>,
    control_records: &mut Vec<String>,
    mutation_records: &mut Vec<String>,
    family_artifact_records: &mut BTreeMap<&'static str, Vec<String>>,
    coverage: &mut CoverageRegistry,
    receipts: &mut Vec<ProductionFeatureReceipt>,
) -> Result<(), String> {
    let expected_operation = metadata_operation_kind(operation);
    if evidence.operation != expected_operation {
        return Err(format!(
            "metadata adapter returned {:?} evidence for {:?}",
            evidence.operation, expected_operation
        ));
    }
    validate_metadata_feature_receipts(
        &evidence.feature_expected,
        &evidence.feature_receipts,
        &evidence.control,
    )?;
    let retained_fixture = metadata_adapter::encode_metadata_fixture(&evidence)?;
    family_artifact_records
        .entry("metadata-fixture.json")
        .or_default()
        .push(format!(
            "{{\"operation\":\"{}\",\"seed\":{seed},\"fixture\":{},\"retained_fixture_schema\":\"{}\",\"retained_fixture_bytes\":{},\"retained_fixture_hex\":\"{}\"}}",
            json_escape(operation.key()),
            metadata_fixture_json(&evidence.fixture),
            metadata_adapter::METADATA_RETAINED_FIXTURE_SCHEMA,
            retained_fixture.len(),
            evidence_hex(&retained_fixture),
        ));
    for query in &evidence.queries {
        let vector_bits = query
            .query_vector_bits
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let sources = query
            .expected_sources
            .iter()
            .map(|source| format!("\"{}\"", json_escape(source)))
            .collect::<Vec<_>>()
            .join(",");
        family_artifact_records
            .entry("queries.jsonl")
            .or_default()
            .push(format!(
                "{{\"campaign\":\"metadata-filter-planner\",\"operation\":\"{}\",\"seed\":{seed},\"query_id\":{},\"phase\":\"{}\",\"predicate\":{},\"query_vector_bits\":[{vector_bits}],\"k\":{},\"tier\":\"{}\",\"expected_sources\":[{sources}]}}",
                json_escape(operation.key()),
                query.query_id,
                json_escape(query.phase),
                metadata_predicate_json(&query.predicate),
                query.k,
                json_escape(query.tier),
            ));
    }
    for (ordinal, mutation) in evidence.fixture_mutations.iter().enumerate() {
        let mutation = metadata_mutation_json(mutation);
        let record = format!(
            "{{\"campaign\":\"metadata-filter-planner\",\"operation\":\"{}\",\"seed\":{seed},\"role\":\"fixture-preparation\",\"ordinal\":{ordinal},\"mutation\":{mutation}}}",
            json_escape(operation.key()),
        );
        family_artifact_records
            .entry("fixture-mutations.jsonl")
            .or_default()
            .push(record.clone());
        mutation_records.push(record);
    }
    let provenance = format!(
        "metadata-filter-planner-oracle-v2 seed={seed} profile={} op={op_index} operation={}",
        profile.key(),
        operation.key()
    );
    let comparison_passed = match &evidence.invariant {
        metadata_adapter::MetadataInvariantEvidence::I36 { input, observed } => {
            if operation != super::campaign::MetadataOperation::Columns {
                return Err("metadata I36 evidence escaped the Columns operation".to_owned());
            }
            let result = metadata_oracle::compare_i36(input, observed);
            let passed = result.is_ok();
            push_metadata_feature_json_record(
                36,
                metadata_oracle::I36_CHECKER_ID,
                operation.key(),
                metadata_i36_input_json(input),
                metadata_i36_observed_json(observed),
                provenance.clone(),
                metadata_oracle::attest_i36(input, observed),
                result,
                oracle_records,
                coverage,
            );
            passed
        }
        metadata_adapter::MetadataInvariantEvidence::I37 { input, observed } => {
            if operation != super::campaign::MetadataOperation::Bitmap {
                return Err("metadata I37 evidence escaped the Bitmap operation".to_owned());
            }
            let result = metadata_oracle::compare_i37(input, observed);
            let passed = result.is_ok();
            push_metadata_feature_json_record(
                37,
                metadata_oracle::I37_CHECKER_ID,
                operation.key(),
                metadata_i37_input_json(input),
                metadata_i37_observed_json(observed),
                provenance.clone(),
                metadata_oracle::attest_i37(input, observed),
                result,
                oracle_records,
                coverage,
            );
            oracle_records
                .last_mut()
                .expect("metadata I37 comparison appended one oracle record")
                .case_identity = Some(metadata_adapter::i37_case_identity(
                op_index,
                evidence.control.seed,
                evidence.fault.map(metadata_adapter_fault_key),
            ));
            passed
        }
        metadata_adapter::MetadataInvariantEvidence::I38 { input, observed } => {
            if operation != super::campaign::MetadataOperation::Planner {
                return Err("metadata I38 evidence escaped the Planner operation".to_owned());
            }
            let result = metadata_oracle::compare_i38(input, observed);
            let passed = result.is_ok();
            push_metadata_feature_json_record(
                38,
                metadata_oracle::I38_CHECKER_ID,
                operation.key(),
                metadata_i38_input_json(input),
                metadata_i38_observed_json(observed),
                provenance.clone(),
                metadata_oracle::attest_i38(input, observed),
                result,
                oracle_records,
                coverage,
            );
            passed
        }
        metadata_adapter::MetadataInvariantEvidence::I39 { expected, observed } => {
            if operation != super::campaign::MetadataOperation::Execution {
                return Err("metadata I39 evidence escaped the Execution operation".to_owned());
            }
            let result = metadata_oracle::compare_i39_expected(expected, observed);
            let passed = result.is_ok();
            push_metadata_feature_json_record(
                39,
                metadata_oracle::I39_CHECKER_ID,
                operation.key(),
                format!(
                    "[{}]",
                    expected
                        .iter()
                        .map(metadata_i39_case_json)
                        .collect::<Vec<_>>()
                        .join(",")
                ),
                metadata_i39_observed_json(observed),
                provenance,
                metadata_oracle::attest_i39(expected, observed),
                result,
                oracle_records,
                coverage,
            );
            passed
        }
    };
    if comparison_passed {
        record_metadata_family_coverage(&evidence, coverage)?;
    }
    let selected_fault = evidence.fault.map_or_else(
        || "null".to_owned(),
        |fault| format!("\"{}\"", metadata_adapter_fault_key(fault)),
    );
    control_records.push(format!(
        "{{\"campaign\":\"metadata-filter-planner\",\"operation\":\"{}\",\"seed\":{seed},\"fault\":{selected_fault},\"control\":{}}}",
        json_escape(operation.key()),
        metadata_control_json(&evidence.control),
    ));
    let mutation = evidence
        .mutation
        .as_ref()
        .map_or_else(|| "null".to_owned(), metadata_mutation_json);
    mutation_records.push(format!(
        "{{\"campaign\":\"metadata-filter-planner\",\"operation\":\"{}\",\"seed\":{seed},\"role\":\"selected-fault\",\"mutation\":{mutation}}}",
        json_escape(operation.key()),
    ));
    if comparison_passed {
        for receipt in evidence.execution_receipts {
            coverage.hit(format!("metadata.branch.{:?}", receipt.branch));
            receipts.push(ProductionFeatureReceipt::MetadataExecution {
                operation: operation.key(),
                receipt,
            });
        }
        receipts.extend(
            evidence
                .feature_receipts
                .into_iter()
                .map(ProductionFeatureReceipt::ValidatedMetadataFeature),
        );
    }
    Ok(())
}

#[cfg(test)]
mod metadata_receipt_credit_tests {
    use super::*;

    #[test]
    fn metadata_execution_receipt_serializes_typed_branch_and_work_facts() {
        let evidence = metadata_adapter::run_metadata_operation(
            metadata_adapter::MetadataOperationKind::Execution,
            0,
            None,
        )
        .expect("observe metadata execution receipt");
        let receipt = evidence
            .execution_receipts
            .first()
            .expect("execution fixture emitted no receipt")
            .clone();
        let line = production_receipt_json(&ProductionFeatureReceipt::MetadataExecution {
            operation: "metadata_execution_truth",
            receipt,
        });
        let record: zeppelin_embed_bench::harness_json::Value =
            zeppelin_embed_bench::harness_json::from_str(&line)
                .expect("parse production receipt evidence");

        assert!(
            record["receipt"].is_object(),
            "metadata execution receipt retained opaque Debug text"
        );
        assert!(record["receipt"]["branch"].is_string());
        assert!(record["receipt"]["rows_examined"].is_u64());
        assert!(record["receipt"]["vectors_scored"].is_u64());
        assert!(record["receipt"]["returned_candidates"].is_u64());
    }

    #[test]
    fn oracle_record_attests_canonical_value_digests_and_first_difference() {
        let mut records = Vec::new();
        let mut coverage = CoverageRegistry::default();
        push_feature_json_record(
            36,
            metadata_oracle::I36_CHECKER_ID,
            "columns",
            "{\"b\":1,\"a\":2}".to_owned(),
            "{\"b\":1,\"a\":2}".to_owned(),
            "schema-plant".to_owned(),
            Ok::<(), &'static str>(()),
            &mut records,
            &mut coverage,
        );
        let line = records.first().expect("one oracle record").json_line();
        let record: zeppelin_embed_bench::harness_json::Value =
            zeppelin_embed_bench::harness_json::from_str(&line).expect("parse oracle record");
        let expected = zeppelin_embed_bench::harness_json::to_vec(&record["expected"])
            .expect("canonical expected JSON");
        let observed = zeppelin_embed_bench::harness_json::to_vec(&record["observed"])
            .expect("canonical observed JSON");

        assert_eq!(
            record["input_digest"].as_str(),
            Some(super::super::artifacts::evidence_digest(&[&expected]).as_str()),
            "oracle input digest is absent or does not bind the canonical primitive"
        );
        assert_eq!(
            record["observed_digest"].as_str(),
            Some(super::super::artifacts::evidence_digest(&[&observed]).as_str()),
            "oracle observation digest is absent or does not bind the canonical primitive"
        );
        assert!(
            record["first_difference"].is_null(),
            "a passing oracle record invented a first difference"
        );
        assert_eq!(record["canonical_version"], 1);
        assert!(record["oracle_input_digest"].is_string());
        assert!(record["oracle_observed_digest"].is_string());

        push_feature_json_record(
            36,
            metadata_oracle::I36_CHECKER_ID,
            "columns",
            "{\"row\":1}".to_owned(),
            "{\"row\":2}".to_owned(),
            "schema-plant".to_owned(),
            Err::<(), &'static str>("expected row=1 observed row=2"),
            &mut records,
            &mut coverage,
        );
        let planted: zeppelin_embed_bench::harness_json::Value =
            zeppelin_embed_bench::harness_json::from_str(&records[1].json_line())
                .expect("parse planted oracle record");
        assert!(
            planted["first_difference"].is_object(),
            "oracle first difference remained an opaque string"
        );
        assert!(planted["first_difference"]["path"].is_string());
        assert!(planted["first_difference"]["kind"].is_string());
    }

    #[test]
    fn metadata_i39_coverage_requires_production_execution_receipts() {
        let mut evidence = metadata_adapter::run_metadata_operation(
            metadata_adapter::MetadataOperationKind::Execution,
            0,
            None,
        )
        .expect("observe metadata execution evidence");
        assert!(
            !evidence.execution_receipts.is_empty(),
            "fixture did not exercise a production execution site"
        );
        evidence.execution_receipts.clear();

        let mut coverage = CoverageRegistry::default();
        record_metadata_family_coverage(&evidence, &mut coverage)
            .expect("missing receipts should not manufacture coverage");

        for key in super::super::campaign::CampaignSpec::for_kind(
            super::super::campaign::CampaignKind::MetadataFilterPlanner,
        )
        .required_coverage
        .iter()
        .copied()
        .filter(|key| {
            key.starts_with("metadata.i39.branch.") || key.starts_with("metadata.i39.fallback.")
        }) {
            assert_eq!(
                coverage.count(key),
                0,
                "expected cases earned I39 coverage without production receipts: {key}"
            );
        }
    }

    #[test]
    fn failed_i37_comparison_cannot_earn_matrix_coverage() {
        let evidence = metadata_adapter::run_metadata_operation(
            metadata_adapter::MetadataOperationKind::Bitmap,
            0,
            None,
        )
        .expect("observe metadata bitmap evidence");
        let evidence = metadata_adapter::apply_metadata_replay_mutation(
            &evidence,
            metadata_adapter::MetadataReplayMutation::OracleObserved,
        )
        .expect("plant one observed bitmap mismatch");
        let mut oracle_records = Vec::new();
        let mut controls = Vec::new();
        let mut mutations = Vec::new();
        let mut family = BTreeMap::new();
        let mut coverage = CoverageRegistry::default();
        let mut receipts = Vec::new();

        record_metadata_evidence(
            super::super::campaign::MetadataOperation::Bitmap,
            evidence,
            0,
            FaultProfile::None,
            0,
            &mut oracle_records,
            &mut controls,
            &mut mutations,
            &mut family,
            &mut coverage,
            &mut receipts,
        )
        .expect("record planted I37 comparison");

        assert_eq!(oracle_records.len(), 1);
        assert!(!oracle_records[0].passed);
        assert_eq!(
            coverage.count("metadata.i37.matrix.eq-u64"),
            0,
            "a failed I37 comparison earned matrix coverage"
        );
    }

    #[test]
    fn metadata_record_consumes_the_family_canonical_attestation() {
        let evidence = metadata_adapter::run_metadata_operation(
            metadata_adapter::MetadataOperationKind::Columns,
            0,
            None,
        )
        .expect("observe metadata columns evidence");
        let metadata_adapter::MetadataInvariantEvidence::I36 { input, observed } =
            &evidence.invariant
        else {
            panic!("columns operation did not return I36 evidence");
        };
        let attestation = metadata_oracle::attest_i36(input, observed);
        let mut oracle_records = Vec::new();
        let mut controls = Vec::new();
        let mut mutations = Vec::new();
        let mut family = BTreeMap::new();
        let mut coverage = CoverageRegistry::default();
        let mut receipts = Vec::new();

        record_metadata_evidence(
            super::super::campaign::MetadataOperation::Columns,
            evidence,
            0,
            FaultProfile::None,
            0,
            &mut oracle_records,
            &mut controls,
            &mut mutations,
            &mut family,
            &mut coverage,
            &mut receipts,
        )
        .expect("record I36 family attestation");

        let record = oracle_records.first().expect("one I36 oracle record");
        assert_eq!(record.canonical_version, attestation.canonical_version);
        assert_eq!(
            record.oracle_input_digest,
            format!(
                "metadata-v{}:{:016x}",
                attestation.canonical_version, attestation.input_digest
            )
        );
        assert_eq!(
            record.oracle_observed_digest,
            format!(
                "metadata-v{}:{:016x}",
                attestation.canonical_version, attestation.observed_digest
            )
        );
        assert_eq!(record.first_difference.is_none(), record.passed);
    }

    fn collect_typed_coverage(
        evidence: &metadata_adapter::MetadataOperationEvidence,
        coverage: &mut CoverageRegistry,
    ) {
        validate_metadata_feature_receipts(
            &evidence.feature_expected,
            &evidence.feature_receipts,
            &evidence.control,
        )
        .expect("metadata receipt facts must validate before coverage");
        record_metadata_family_coverage(evidence, coverage)
            .expect("metadata typed evidence must earn coverage");
    }

    #[test]
    fn typed_metadata_evidence_reaches_every_required_family_coverage_key() {
        let mut coverage = CoverageRegistry::default();
        for seed in 0..metadata_adapter::I37_PREDICATE_CASE_COUNT {
            let evidence = metadata_adapter::run_metadata_operation(
                metadata_adapter::MetadataOperationKind::Bitmap,
                seed,
                None,
            )
            .expect("observe metadata predicate grammar");
            collect_typed_coverage(&evidence, &mut coverage);
        }
        for seed in 0..3 {
            let evidence = metadata_adapter::run_metadata_operation(
                metadata_adapter::MetadataOperationKind::Planner,
                seed,
                None,
            )
            .expect("observe metadata pruning topology");
            collect_typed_coverage(&evidence, &mut coverage);
        }
        for seed in 0..3 {
            let evidence = metadata_adapter::run_metadata_operation(
                metadata_adapter::MetadataOperationKind::Columns,
                seed,
                Some(metadata_adapter::MetadataFaultKind::ColumnCorruption),
            )
            .expect("observe metadata column corruption class");
            collect_typed_coverage(&evidence, &mut coverage);
        }
        for fault in [
            metadata_adapter::MetadataFaultKind::BitmapTruncation,
            metadata_adapter::MetadataFaultKind::SelectivityBoundary,
            metadata_adapter::MetadataFaultKind::VisitedBudgetFallback,
        ] {
            let operation = match fault {
                metadata_adapter::MetadataFaultKind::BitmapTruncation => {
                    metadata_adapter::MetadataOperationKind::Bitmap
                }
                metadata_adapter::MetadataFaultKind::SelectivityBoundary
                | metadata_adapter::MetadataFaultKind::VisitedBudgetFallback => {
                    metadata_adapter::MetadataOperationKind::Execution
                }
                metadata_adapter::MetadataFaultKind::ColumnCorruption => {
                    unreachable!("column corruption is covered by the seed loop")
                }
            };
            let evidence = metadata_adapter::run_metadata_operation(operation, 0, Some(fault))
                .expect("observe metadata fault coverage");
            collect_typed_coverage(&evidence, &mut coverage);
        }
        let execution = metadata_adapter::run_metadata_operation(
            metadata_adapter::MetadataOperationKind::Execution,
            0,
            None,
        )
        .expect("observe metadata branch and fallback catalog");
        collect_typed_coverage(&execution, &mut coverage);

        let missing = super::super::campaign::CampaignSpec::for_kind(
            super::super::campaign::CampaignKind::MetadataFilterPlanner,
        )
        .required_coverage
        .iter()
        .copied()
        .filter(|key| coverage.count(key) == 0)
        .collect::<Vec<_>>();
        assert!(
            missing.is_empty(),
            "missing typed metadata coverage: {missing:?}"
        );
    }

    #[test]
    fn mismatched_metadata_guard_facts_cannot_earn_fault_credit() {
        let mut evidence = metadata_adapter::run_metadata_operation(
            metadata_adapter::MetadataOperationKind::Execution,
            0x39,
            Some(metadata_adapter::MetadataFaultKind::SelectivityBoundary),
        )
        .expect("observe metadata selectivity fault");
        let Some(metadata_adapter::MetadataFeatureExpected::SelectivityBoundaryChosen {
            cardinality,
            ..
        }) = evidence.feature_expected.first_mut()
        else {
            panic!("selectivity evidence omitted its first expected receipt");
        };
        *cardinality = 2;

        let mut oracle_records = Vec::new();
        let mut control_records = Vec::new();
        let mut mutation_records = Vec::new();
        let mut family_artifact_records = BTreeMap::new();
        let mut coverage = CoverageRegistry::default();
        let mut receipts = Vec::new();

        let error = record_metadata_evidence(
            super::super::campaign::MetadataOperation::Execution,
            evidence,
            0x39,
            FaultProfile::None,
            0,
            &mut oracle_records,
            &mut control_records,
            &mut mutation_records,
            &mut family_artifact_records,
            &mut coverage,
            &mut receipts,
        )
        .expect_err("mismatched metadata receipt facts were accepted");

        assert!(error.contains("metadata feature receipt"), "{error}");
        assert!(
            receipts.is_empty(),
            "mismatched receipt earned fault credit"
        );
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the shared adapter boundary carries every replay evidence stream"
)]
fn run_metadata_campaign_operation(
    operation: super::campaign::MetadataOperation,
    selected_faults: &[super::campaign::FeatureFault],
    seed: u64,
    profile: FaultProfile,
    op_index: usize,
    oracle_records: &mut Vec<OracleRecord>,
    control_records: &mut Vec<String>,
    mutation_records: &mut Vec<String>,
    family_artifact_records: &mut BTreeMap<&'static str, Vec<String>>,
    coverage: &mut CoverageRegistry,
) -> Result<Vec<ProductionFeatureReceipt>, String> {
    for name in super::artifacts::METADATA_REPLAY_ARTIFACTS {
        family_artifact_records.entry(name).or_default();
    }
    let matching_faults = selected_faults
        .iter()
        .copied()
        .filter(|fault| fault.operation() == super::campaign::FeatureOperation::Metadata(operation))
        .map(metadata_fault_kind)
        .map(|fault| fault.map(Some))
        .collect::<Result<Vec<_>, _>>()?;
    let cases = if matching_faults.is_empty() {
        vec![None]
    } else {
        matching_faults
    };
    let mut receipts = Vec::new();
    if operation == super::campaign::MetadataOperation::Bitmap {
        for case in metadata_adapter::run_i37_complete_public_matrix(seed)? {
            let case_seed = case.evidence.control.seed;
            record_metadata_evidence(
                operation,
                case.evidence,
                case_seed,
                profile,
                op_index,
                oracle_records,
                control_records,
                mutation_records,
                family_artifact_records,
                coverage,
                &mut receipts,
            )?;
        }
    }
    for fault in cases {
        if operation == super::campaign::MetadataOperation::Bitmap && fault.is_none() {
            continue;
        }
        let evidence = metadata_adapter::run_metadata_operation(
            metadata_operation_kind(operation),
            seed,
            fault,
        )?;
        record_metadata_evidence(
            operation,
            evidence,
            seed,
            profile,
            op_index,
            oracle_records,
            control_records,
            mutation_records,
            family_artifact_records,
            coverage,
            &mut receipts,
        )?;
    }
    Ok(receipts)
}

#[cfg(test)]
fn metadata_replay_self_test_streams(
    evidence: metadata_adapter::MetadataOperationEvidence,
) -> Result<BTreeMap<&'static str, Vec<u8>>, String> {
    let mut oracle_records = Vec::new();
    let mut control_records = Vec::new();
    let mut mutation_records = Vec::new();
    let mut family_records = BTreeMap::new();
    let mut coverage = CoverageRegistry::default();
    let mut receipts = Vec::new();
    record_metadata_evidence(
        super::campaign::MetadataOperation::Execution,
        evidence,
        0,
        FaultProfile::None,
        0,
        &mut oracle_records,
        &mut control_records,
        &mut mutation_records,
        &mut family_records,
        &mut coverage,
        &mut receipts,
    )?;

    let line_stream = |lines: Vec<String>| {
        let mut bytes = Vec::new();
        for line in lines {
            bytes.extend_from_slice(line.as_bytes());
            bytes.push(b'\n');
        }
        bytes
    };
    let mut streams = BTreeMap::from([
        (
            "oracle.jsonl",
            line_stream(oracle_records.iter().map(OracleRecord::json_line).collect()),
        ),
        ("controls.jsonl", line_stream(control_records)),
        (
            "receipts.jsonl",
            line_stream(receipts.iter().map(production_receipt_json).collect()),
        ),
        ("mutations.jsonl", line_stream(mutation_records)),
    ]);
    for (name, records) in family_records {
        streams.insert(name, render_family_artifact(name, &records)?);
    }
    Ok(streams)
}

#[cfg(test)]
fn compare_metadata_replay_stream(
    expected: &BTreeMap<&'static str, Vec<u8>>,
    observed: &BTreeMap<&'static str, Vec<u8>>,
    artifact: &'static str,
) -> Result<(), String> {
    let expected = expected
        .get(artifact)
        .ok_or_else(|| format!("metadata replay expected artifact {artifact} is absent"))?;
    let observed = observed
        .get(artifact)
        .ok_or_else(|| format!("metadata replay observed artifact {artifact} is absent"))?;
    if expected == observed {
        return Ok(());
    }
    let byte_offset = expected
        .iter()
        .zip(observed)
        .position(|(left, right)| left != right)
        .unwrap_or_else(|| expected.len().min(observed.len()));
    Err(format!(
        "metadata replay mismatch artifact={artifact} byte_offset={byte_offset}"
    ))
}

/// Exercises every family-provided metadata replay mutation through the shared
/// canonical serializers and the same fail-closed artifact comparison used by
/// replay. Every plant must be rejected with its exact artifact identity.
#[cfg(test)]
pub fn metadata_replay_mutation_self_test() -> Result<Vec<&'static str>, String> {
    let evidence = metadata_adapter::run_metadata_operation(
        metadata_adapter::MetadataOperationKind::Execution,
        0,
        None,
    )?;
    let baseline = metadata_replay_self_test_streams(evidence.clone())?;
    let mut mismatches = Vec::new();
    for (mutation, stream) in [
        (
            metadata_adapter::MetadataReplayMutation::OracleObserved,
            "oracle.jsonl",
        ),
        (
            metadata_adapter::MetadataReplayMutation::ExecutionReceipt,
            "receipts.jsonl",
        ),
        (
            metadata_adapter::MetadataReplayMutation::MutationByte,
            "fixture-mutations.jsonl",
        ),
        (
            metadata_adapter::MetadataReplayMutation::SameSeedDigest,
            "controls.jsonl",
        ),
    ] {
        let mutated = metadata_adapter::apply_metadata_replay_mutation(&evidence, mutation)?;
        let mutated = match metadata_replay_self_test_streams(mutated) {
            Ok(streams) => streams,
            Err(error)
                if mutation == metadata_adapter::MetadataReplayMutation::SameSeedDigest
                    && error.contains("same-seed fixture directories are not byte-identical") =>
            {
                mismatches.push(stream);
                continue;
            }
            Err(error) => return Err(error),
        };
        let error = compare_metadata_replay_stream(&baseline, &mutated, stream)
            .expect_err("metadata replay accepted a family-owned mutation");
        if !error.contains(&format!("artifact={stream}")) {
            return Err(format!(
                "metadata replay mutation {mutation:?} returned an unnamed mismatch: {error}"
            ));
        }
        mismatches.push(stream);
    }
    Ok(mismatches)
}

fn vector_operation_kind(
    operation: super::campaign::VectorOperation,
) -> vector_adapter::VectorOperationKind {
    match operation {
        super::campaign::VectorOperation::KernelParity => {
            vector_adapter::VectorOperationKind::KernelParity
        }
        super::campaign::VectorOperation::Quantization => {
            vector_adapter::VectorOperationKind::Quantization
        }
        super::campaign::VectorOperation::Rescore => vector_adapter::VectorOperationKind::Rescore,
        super::campaign::VectorOperation::RowIdentity => {
            vector_adapter::VectorOperationKind::RowIdentity
        }
    }
}

fn vector_fault_kind(
    fault: super::campaign::FeatureFault,
) -> Result<vector_adapter::VectorFaultKind, String> {
    match fault {
        super::campaign::FeatureFault::VectorForcedDispatchBackend => {
            Ok(vector_adapter::VectorFaultKind::ForcedDispatchBackend)
        }
        super::campaign::FeatureFault::VectorCorruptCodesFactors => {
            Ok(vector_adapter::VectorFaultKind::CorruptCodesFactors)
        }
        super::campaign::FeatureFault::VectorMissingRescoreRows => {
            Ok(vector_adapter::VectorFaultKind::MissingRescoreRows)
        }
        super::campaign::FeatureFault::VectorRowCountCancellation => {
            Ok(vector_adapter::VectorFaultKind::RowCountCancellation)
        }
        super::campaign::FeatureFault::VectorAllocationDenial => {
            Ok(vector_adapter::VectorFaultKind::AllocationDenial)
        }
        other => Err(format!(
            "vector operation received unrelated fault {}",
            other.key()
        )),
    }
}

fn vector_backend_key(backend: vector_oracle::BackendId) -> &'static str {
    match backend {
        vector_oracle::BackendId::Scalar => "scalar",
        vector_oracle::BackendId::NeonWiden => "neon-widen",
        vector_oracle::BackendId::NeonDotprodU4 => "neon-dotprod-u4",
        vector_oracle::BackendId::NeonI8mm => "neon-i8mm",
        vector_oracle::BackendId::NeonDotprodU2 => "neon-dotprod-u2",
        vector_oracle::BackendId::NeonDotprodU6 => "neon-dotprod-u6",
        vector_oracle::BackendId::NeonDotprodU8 => "neon-dotprod-u8",
        vector_oracle::BackendId::NeonDotprodU4Prefetch => "neon-dotprod-u4-prefetch",
        vector_oracle::BackendId::Avx2 => "avx2",
    }
}

fn product_vector_backend(backend: vector_oracle::BackendId) -> KernelBackendId {
    match backend {
        vector_oracle::BackendId::Scalar => KernelBackendId::Scalar,
        vector_oracle::BackendId::NeonWiden => KernelBackendId::NeonWiden,
        vector_oracle::BackendId::NeonDotprodU4 => KernelBackendId::NeonDotprodU4,
        vector_oracle::BackendId::NeonI8mm => KernelBackendId::NeonI8mm,
        vector_oracle::BackendId::NeonDotprodU2 => KernelBackendId::NeonDotprodU2,
        vector_oracle::BackendId::NeonDotprodU6 => KernelBackendId::NeonDotprodU6,
        vector_oracle::BackendId::NeonDotprodU8 => KernelBackendId::NeonDotprodU8,
        vector_oracle::BackendId::NeonDotprodU4Prefetch => KernelBackendId::NeonDotprodU4Prefetch,
        vector_oracle::BackendId::Avx2 => KernelBackendId::Avx2,
    }
}

fn product_vector_kernel(
    kernel: vector_oracle::KernelId,
) -> zeppelin_embed::kernels::vector_fault::KernelOperationId {
    use zeppelin_embed::kernels::vector_fault::KernelOperationId as Product;
    match kernel {
        vector_oracle::KernelId::DotI8 => Product::DotI8,
        vector_oracle::KernelId::HammingU1 => Product::HammingU1,
        vector_oracle::KernelId::DotF32 => Product::DotF32,
        vector_oracle::KernelId::DotF16 => Product::DotF16,
        vector_oracle::KernelId::DotI8Batch => Product::DotI8Batch,
        vector_oracle::KernelId::HammingU1Batch => Product::HammingU1Batch,
        vector_oracle::KernelId::DotBit4 => Product::DotBit4,
        vector_oracle::KernelId::DotBit4Prepared => Product::DotBit4Prepared,
        vector_oracle::KernelId::DotBit4Batch => Product::DotBit4Batch,
        vector_oracle::KernelId::ScoreBit4PreparedBatch => Product::ScoreBit4PreparedBatch,
        vector_oracle::KernelId::ScoreBit4Ptrs => Product::ScoreBit4Ptrs,
    }
}

fn product_vector_source(source: vector_oracle::PrimitiveSource) -> ProductVectorRowSource {
    match source {
        vector_oracle::PrimitiveSource::Active => ProductVectorRowSource::Active,
        vector_oracle::PrimitiveSource::Sealed(segment) => ProductVectorRowSource::Sealed(segment),
    }
}

fn product_vector_tier(tier: u8) -> Result<ProductVectorSearchTier, String> {
    match tier {
        0 => Ok(ProductVectorSearchTier::Auto),
        1 => Ok(ProductVectorSearchTier::Exact),
        2 => Ok(ProductVectorSearchTier::Scan),
        3 => Ok(ProductVectorSearchTier::Graph),
        other => Err(format!("unknown vector tier id {other}")),
    }
}

fn vector_campaign_key(campaign: ProductVectorCampaign) -> &'static str {
    match campaign {
        ProductVectorCampaign::VectorExecution => "vector-execution",
    }
}

fn vector_operation_key(operation: ProductVectorOperation) -> &'static str {
    match operation {
        ProductVectorOperation::KernelParity => "kernel-parity",
        ProductVectorOperation::Quantization => "quantization",
        ProductVectorOperation::Rescore => "rescore",
        ProductVectorOperation::RowIdentity => "row-identity",
    }
}

fn vector_fault_key(fault: ProductVectorFaultKind) -> &'static str {
    match fault {
        ProductVectorFaultKind::ForcedDispatchBackend => "forced-dispatch-backend",
        ProductVectorFaultKind::CorruptCodesFactors => "corrupt-codes-factors",
        ProductVectorFaultKind::MissingRescoreRows => "missing-rescore-rows",
        ProductVectorFaultKind::RowCountCancellation => "row-count-cancellation",
        ProductVectorFaultKind::AllocationDenial => "allocation-denial",
    }
}

fn vector_site_key(site: ProductVectorFaultSite) -> &'static str {
    match site {
        ProductVectorFaultSite::KernelDispatchSelectedScoringTable => {
            "KernelDispatchSelectedScoringTable"
        }
        ProductVectorFaultSite::ScanBit4CodeView => "ScanBit4CodeView",
        ProductVectorFaultSite::ScanBit4FactorView => "ScanBit4FactorView",
        ProductVectorFaultSite::ScanInt8FactorView => "ScanInt8FactorView",
        ProductVectorFaultSite::ExactRescoreRows => "ExactRescoreRows",
        ProductVectorFaultSite::QueryRescoreRows => "QueryRescoreRows",
        ProductVectorFaultSite::ScoredVectorRow => "ScoredVectorRow",
        ProductVectorFaultSite::SearchGlobalCandidates => "SearchGlobalCandidates",
    }
}

fn product_vector_tier_key(tier: ProductVectorSearchTier) -> &'static str {
    match tier {
        ProductVectorSearchTier::Auto => "auto",
        ProductVectorSearchTier::Exact => "exact",
        ProductVectorSearchTier::Scan => "scan",
        ProductVectorSearchTier::Graph => "graph",
    }
}

fn product_vector_source_json(source: ProductVectorRowSource) -> String {
    match source {
        ProductVectorRowSource::Active => "{\"kind\":\"active\"}".to_owned(),
        ProductVectorRowSource::Sealed(segment) => format!(
            "{{\"kind\":\"sealed\",\"segment\":\"{}\"}}",
            evidence_hex(&segment)
        ),
    }
}

fn vector_effect_json(effect: &ProductVectorFaultEffect) -> String {
    match effect {
        ProductVectorFaultEffect::ForcedBackend {
            requested,
            selected,
            kernel,
            work_items,
        } => format!(
            "{{\"kind\":\"forced-backend\",\"requested\":\"{}\",\"selected\":\"{}\",\"kernel\":\"{:?}\",\"work_items\":{work_items}}}",
            requested.as_str(),
            selected.as_str(),
            kernel,
        ),
        ProductVectorFaultEffect::CorruptedPayload {
            scheme,
            source,
            tier,
            local_row,
            field,
            byte_offset,
            before_bits,
            after_bits,
        } => format!(
            "{{\"kind\":\"corrupted-payload\",\"scheme\":\"{:?}\",\"source\":{},\"tier\":\"{}\",\"local_row\":{local_row},\"field\":\"{:?}\",\"byte_offset\":{byte_offset},\"before_bits\":{before_bits},\"after_bits\":{after_bits}}}",
            scheme,
            product_vector_source_json(*source),
            product_vector_tier_key(*tier),
            field,
        ),
        ProductVectorFaultEffect::MissingRescoreRows {
            segment,
            expected_rows,
            available_rows,
            requested_tier,
        } => format!(
            "{{\"kind\":\"missing-rescore-rows\",\"segment\":\"{}\",\"expected_rows\":{expected_rows},\"available_rows\":{available_rows},\"requested_tier\":\"{}\"}}",
            evidence_hex(segment),
            product_vector_tier_key(*requested_tier),
        ),
        ProductVectorFaultEffect::CancelledAfterRows {
            source,
            requested_tier,
            local_row,
            requested_rows,
            observed_rows,
        } => format!(
            "{{\"kind\":\"cancelled-after-rows\",\"source\":{},\"requested_tier\":\"{}\",\"local_row\":{local_row},\"requested_rows\":{requested_rows},\"observed_rows\":{observed_rows}}}",
            product_vector_source_json(*source),
            product_vector_tier_key(*requested_tier),
        ),
        ProductVectorFaultEffect::AllocationDenied {
            component,
            requested_tier,
            items,
            bytes,
        } => format!(
            "{{\"kind\":\"allocation-denied\",\"component\":\"{:?}\",\"requested_tier\":\"{}\",\"items\":{items},\"bytes\":{bytes}}}",
            component,
            product_vector_tier_key(*requested_tier),
        ),
    }
}

fn vector_quant_success(
    fixture: &vector_adapter::VectorPrimitiveFixture,
    scheme: vector_oracle::QuantScheme,
) -> Result<vector_oracle::I25Expected, String> {
    let vector_adapter::VectorPrimitiveInputs::I25(inputs) = &fixture.inputs else {
        return Err("quantization mutation is not backed by I25 primitive inputs".to_owned());
    };
    let mut input = inputs
        .iter()
        .find(|input| {
            input.scheme == vector_oracle::QuantScheme::Bit4
                && input.store.schedule == [vector_oracle::PublicStoreStep::IngestAccepted]
        })
        .cloned()
        .ok_or_else(|| {
            "quantization fixture omitted its planned local-row-zero input".to_owned()
        })?;
    input.scheme = scheme;
    let encoded_len = match scheme {
        vector_oracle::QuantScheme::Bit4 => input.row.len().div_ceil(2),
        vector_oracle::QuantScheme::Int8 => input.row.len(),
    } as u64;
    input.output_len = encoded_len;
    input.code_len = encoded_len;
    let expected = vector_oracle::expected_quantization(&input);
    if expected.result.success.is_none() {
        return Err(format!(
            "quantization mutation selected unsuccessful primitive input {}",
            input.case_id
        ));
    }
    Ok(expected)
}

fn expected_vector_quant_effect(
    fixture: &vector_adapter::VectorPrimitiveFixture,
    case_id: u64,
    scheme: vector_oracle::QuantScheme,
    source: vector_oracle::PrimitiveSource,
    tier: u8,
    local_row: u32,
    field: vector_adapter::VectorQuantMutationField,
) -> Result<(ProductVectorFaultSite, ProductVectorFaultEffect), String> {
    let expected = vector_quant_success(fixture, scheme)?;
    let success = expected
        .result
        .success
        .as_ref()
        .ok_or_else(|| "successful quantization facts disappeared".to_owned())?;
    let product_tier = product_vector_tier(tier)?;
    let product_source = product_vector_source(source);
    match field {
        vector_adapter::VectorQuantMutationField::Bit4OddPadding => {
            if scheme != vector_oracle::QuantScheme::Bit4 {
                return Err("Bit4 padding mutation named another quantization scheme".to_owned());
            }
            let byte_offset = success
                .code_bytes
                .len()
                .checked_sub(1)
                .ok_or_else(|| "Bit4 padding mutation has no code byte".to_owned())?;
            let before = success.code_bytes[byte_offset];
            let after = before | 0x0f;
            let _ = case_id;
            Ok((
                ProductVectorFaultSite::ScanBit4CodeView,
                ProductVectorFaultEffect::CorruptedPayload {
                    scheme: ProductVectorQuantScheme::Bit4,
                    source: product_source,
                    tier: product_tier,
                    local_row,
                    field: ProductVectorQuantField::OddPadding,
                    byte_offset: byte_offset as u64,
                    before_bits: u64::from(before),
                    after_bits: u64::from(after),
                },
            ))
        }
        vector_adapter::VectorQuantMutationField::Bit4Correction => {
            if scheme != vector_oracle::QuantScheme::Bit4 {
                return Err("Bit4 correction mutation named another quantization scheme".to_owned());
            }
            let before = success
                .factor_bits
                .get(2)
                .ok_or_else(|| "Bit4 correction mutation omitted the correction factor".to_owned())?
                .0;
            let after = f32::NAN.to_bits();
            Ok((
                ProductVectorFaultSite::ScanBit4FactorView,
                ProductVectorFaultEffect::CorruptedPayload {
                    scheme: ProductVectorQuantScheme::Bit4,
                    source: product_source,
                    tier: product_tier,
                    local_row,
                    field: ProductVectorQuantField::Correction,
                    byte_offset: 8,
                    before_bits: u64::from(before),
                    after_bits: u64::from(after),
                },
            ))
        }
        vector_adapter::VectorQuantMutationField::Int8Scale => {
            if scheme != vector_oracle::QuantScheme::Int8 {
                return Err("Int8 scale mutation named another quantization scheme".to_owned());
            }
            let before = success
                .factor_bits
                .first()
                .ok_or_else(|| "Int8 scale mutation omitted its scale".to_owned())?
                .0;
            let after = f32::NAN.to_bits();
            Ok((
                ProductVectorFaultSite::ScanInt8FactorView,
                ProductVectorFaultEffect::CorruptedPayload {
                    scheme: ProductVectorQuantScheme::Int8,
                    source: product_source,
                    tier: product_tier,
                    local_row,
                    field: ProductVectorQuantField::Scale,
                    byte_offset: 0,
                    before_bits: u64::from(before),
                    after_bits: u64::from(after),
                },
            ))
        }
    }
}

fn validate_vector_receipt_header(
    receipt: &VectorFaultReceipt,
    operation: ProductVectorOperation,
    fault: ProductVectorFaultKind,
    site: ProductVectorFaultSite,
    case_id: u64,
    result_published: bool,
) -> Result<(), String> {
    if receipt.campaign() == ProductVectorCampaign::VectorExecution
        && receipt.operation() == operation
        && receipt.fault() == fault
        && receipt.site() == site
        && receipt.cardinality() == 1
        && receipt.seed_case_id() == case_id
        && receipt.result_published() == result_published
    {
        Ok(())
    } else {
        Err(format!(
            "typed vector receipt header mismatch: expected operation={operation:?} fault={fault:?} site={site:?} case={case_id} cardinality=1 result_published={result_published}, observed={receipt:?}"
        ))
    }
}

fn vector_paired_sources_match(
    operation_source: vector_oracle::PrimitiveSource,
    paired_source: vector_oracle::PrimitiveSource,
) -> bool {
    matches!(
        (operation_source, paired_source),
        (
            vector_oracle::PrimitiveSource::Active,
            vector_oracle::PrimitiveSource::Active
        ) | (
            vector_oracle::PrimitiveSource::Sealed(_),
            vector_oracle::PrimitiveSource::Sealed(_)
        )
    )
}

fn validate_vector_paired_mutation(
    operation: &vector_adapter::VectorMutationEvidence,
    paired: &vector_adapter::VectorMutationEvidence,
) -> Result<(), String> {
    let aligned = match (operation, paired) {
        (
            vector_adapter::VectorMutationEvidence::None,
            vector_adapter::VectorMutationEvidence::None,
        ) => true,
        (
            vector_adapter::VectorMutationEvidence::ForcedDispatch {
                case_id: operation_case,
                requested: operation_requested,
            },
            vector_adapter::VectorMutationEvidence::ForcedDispatch {
                case_id: paired_case,
                requested: paired_requested,
            },
        ) => operation_case == paired_case && operation_requested == paired_requested,
        (
            vector_adapter::VectorMutationEvidence::QuantCorruption {
                case_id: operation_case,
                scheme: operation_scheme,
                source: operation_source,
                tier: operation_tier,
                local_row: operation_row,
                field: operation_field,
            },
            vector_adapter::VectorMutationEvidence::QuantCorruption {
                case_id: paired_case,
                scheme: paired_scheme,
                source: paired_source,
                tier: paired_tier,
                local_row: paired_row,
                field: paired_field,
            },
        ) => {
            operation_case == paired_case
                && operation_scheme == paired_scheme
                && vector_paired_sources_match(*operation_source, *paired_source)
                && operation_tier == paired_tier
                && operation_row == paired_row
                && operation_field == paired_field
        }
        (
            vector_adapter::VectorMutationEvidence::MissingRescoreRows {
                case_id: operation_case,
                source: operation_source,
                tier: operation_tier,
                site: operation_site,
                expected_rows: operation_expected,
                available_rows: operation_available,
            },
            vector_adapter::VectorMutationEvidence::MissingRescoreRows {
                case_id: paired_case,
                source: paired_source,
                tier: paired_tier,
                site: paired_site,
                expected_rows: paired_expected,
                available_rows: paired_available,
            },
        ) => {
            operation_case == paired_case
                && vector_paired_sources_match(*operation_source, *paired_source)
                && operation_tier == paired_tier
                && operation_site == paired_site
                && operation_expected == paired_expected
                && operation_available == paired_available
        }
        (
            vector_adapter::VectorMutationEvidence::RowCancellation {
                case_id: operation_case,
                source: operation_source,
                tier: operation_tier,
                requested_rows: operation_rows,
            },
            vector_adapter::VectorMutationEvidence::RowCancellation {
                case_id: paired_case,
                source: paired_source,
                tier: paired_tier,
                requested_rows: paired_rows,
            },
        ) => {
            operation_case == paired_case
                && vector_paired_sources_match(*operation_source, *paired_source)
                && operation_tier == paired_tier
                && operation_rows == paired_rows
        }
        (
            vector_adapter::VectorMutationEvidence::AllocationDenial {
                case_id: operation_case,
                component: operation_component,
                items: operation_items,
                bytes: operation_bytes,
            },
            vector_adapter::VectorMutationEvidence::AllocationDenial {
                case_id: paired_case,
                component: paired_component,
                items: paired_items,
                bytes: paired_bytes,
            },
        ) => {
            operation_case == paired_case
                && operation_component == paired_component
                && operation_items == paired_items
                && operation_bytes == paired_bytes
        }
        _ => false,
    };
    if aligned {
        Ok(())
    } else {
        Err(format!(
            "vector paired fault/mutation mismatch: operation={operation:?} paired={paired:?}"
        ))
    }
}

fn validate_vector_receipts(
    evidence: &vector_adapter::VectorOperationEvidence,
) -> Result<Vec<ProductionFeatureReceipt>, String> {
    if evidence.control.operation != evidence.operation
        || evidence.control.seed != evidence.fixture.seed
        || evidence.fixture.operation != evidence.operation.key()
        || evidence.control.namespace
            != format!("vector-execution/{}", evidence.operation.key()).as_str()
    {
        return Err(format!(
            "vector fixture/control identity mismatch: fixture={:?} control={:?}",
            evidence.fixture, evidence.control
        ));
    }
    if !evidence.control.isolated_directories {
        return Err("vector same-seed control did not use isolated directories".to_owned());
    }
    if evidence.control.clean_initial_directory.files.is_empty()
        || evidence.control.fault_initial_directory.files.is_empty()
    {
        return Err("vector same-seed fixture directory inventory is empty".to_owned());
    }
    if evidence.control.clean_initial_directory != evidence.control.fault_initial_directory {
        return Err(format!(
            "vector same-seed fixture directories are not byte-identical: clean={:?} fault={:?}",
            evidence.control.clean_initial_directory, evidence.control.fault_initial_directory
        ));
    }
    if evidence.control.clean != evidence.control.retry {
        return Err(format!(
            "vector same-seed clear retry diverged from clean control: clean={:?} retry={:?}",
            evidence.control.clean, evidence.control.retry
        ));
    }
    let (receipts, clean_result, fault_result) = if let Some(generic) = &evidence.generic_fault {
        if generic.operation != evidence.operation
            || generic.feature_fault != evidence.fault
            || generic.program_op_index != generic.clean.event.op_index
            || generic.program_op_index != generic.fault.event.op_index
            || generic.clean.event.id != generic.schedule.id
            || generic.fault.event.id != generic.schedule.id
            || generic.clean.event.site != generic.schedule.site
            || generic.fault.event.site != generic.schedule.site
            || generic.clean.event.mode != generic.schedule.mode
            || generic.fault.event.mode != generic.schedule.mode
            || generic.clean.event.nth_match != generic.schedule.nth_match
            || generic.fault.event.nth_match != generic.schedule.nth_match
            || generic.clean.event.path_contains != generic.schedule.path_contains
            || generic.fault.event.path_contains != generic.schedule.path_contains
        {
            return Err(format!(
                "vector generic fault schedule/evidence mismatch: {generic:?}"
            ));
        }
        if !generic.isolated_directories
            || !generic.isolated_runtimes
            || generic.clean_initial_directory.files.is_empty()
            || generic.fault_initial_directory.files.is_empty()
            || generic.clean_initial_directory != generic.fault_initial_directory
        {
            return Err(format!(
                "vector generic fault pair was not isolated and byte-identical: {generic:?}"
            ));
        }
        if !generic.clean.event.fired
            || !generic.fault.event.fired
            || generic.clean.event.path != generic.fault.event.path
        {
            return Err(format!(
                "vector generic fault did not fire identically in both legs: clean={:?} fault={:?}",
                generic.clean.event, generic.fault.event
            ));
        }
        if !generic.clean.feature_receipts.is_empty() {
            return Err("vector generic clean leg emitted a typed feature receipt".to_owned());
        }
        if evidence.fault.is_some() && generic.fault.feature_receipts.is_empty() {
            return Err(
                "generic fault blocked the vector feature site before its typed receipt".to_owned(),
            );
        }
        if evidence.fault.is_none() && !generic.fault.feature_receipts.is_empty() {
            return Err("unarmed vector generic fault leg emitted a feature receipt".to_owned());
        }
        let vector_adapter::VectorGenericFaultStatus::StoreResult(clean) = &generic.clean.status
        else {
            return Err(format!(
                "vector generic clean leg did not reach its public query: {:?}",
                generic.clean.status
            ));
        };
        let vector_adapter::VectorGenericFaultStatus::StoreResult(fault) = &generic.fault.status
        else {
            return Err(format!(
                "vector generic fault leg emitted feature credit without a public query result: {:?}",
                generic.fault.status
            ));
        };
        (generic.fault.feature_receipts.as_slice(), clean, fault)
    } else {
        (
            evidence.receipts.as_slice(),
            &evidence.control.clean,
            &evidence.control.fault,
        )
    };
    let mutation = if let Some(generic) = &evidence.generic_fault {
        validate_vector_paired_mutation(&evidence.mutation, &generic.feature_mutation)?;
        &generic.feature_mutation
    } else {
        &evidence.mutation
    };
    let mut validated = Vec::new();
    match (&evidence.fault, mutation) {
        (None, vector_adapter::VectorMutationEvidence::None) => {
            if !receipts.is_empty() || evidence.forced_child.is_some() {
                return Err("clean vector operation emitted a production fault receipt".to_owned());
            }
            if evidence.control.clean != evidence.control.fault {
                return Err("clean vector operation changed its fault-leg control".to_owned());
            }
        }
        (
            Some(vector_adapter::VectorFaultKind::ForcedDispatchBackend),
            vector_adapter::VectorMutationEvidence::ForcedDispatch { case_id, requested },
        ) => {
            if evidence.generic_fault.is_none() && !receipts.is_empty() {
                return Err("forced backend child leaked an in-process receipt".to_owned());
            }
            let child = evidence
                .forced_child
                .as_ref()
                .ok_or_else(|| "forced backend fault omitted fresh-child evidence".to_owned())?;
            let expected_effect = ProductVectorFaultEffect::ForcedBackend {
                requested: product_vector_backend(*requested),
                selected: product_vector_backend(child.pair.observed.backend),
                kernel: product_vector_kernel(child.pair.observed.kernel),
                work_items: child.pair.observed.work_items,
            };
            if child.requested != *requested
                || child.transport != vector_adapter::ForcedBackendTransportFormat::TypedBinaryV1
                || child.pair.input.case_id != *case_id
                || child.pair.input.backend != *requested
                || child.pair.observed.case_id != *case_id
                || child.pair.observed.backend != *requested
                || !child.pair.input.selected_for_store
                || !child.pair.observed.selected_for_store
                || child.fault_result != evidence.control.fault
                || child.retry_result != evidence.control.retry
                || evidence.control.clean != evidence.control.fault
                || child.receipt.campaign() != ProductVectorCampaign::VectorExecution
                || child.receipt.operation() != ProductVectorOperation::KernelParity
                || child.receipt.fault() != ProductVectorFaultKind::ForcedDispatchBackend
                || child.receipt.site()
                    != ProductVectorFaultSite::KernelDispatchSelectedScoringTable
                || child.receipt.cardinality() != 1
                || child.receipt.seed_case_id() != *case_id
                || child.receipt.effect() != &expected_effect
                || !child.receipt.result_published()
            {
                return Err(format!(
                    "forced backend child receipt/facts mismatch: mutation={:?} child={child:?}",
                    mutation
                ));
            }
            if evidence.generic_fault.is_some() {
                let [receipt] = receipts else {
                    return Err(format!(
                        "paired forced-backend fault emitted {} production receipts, expected one",
                        receipts.len()
                    ));
                };
                validate_vector_receipt_header(
                    receipt,
                    ProductVectorOperation::KernelParity,
                    ProductVectorFaultKind::ForcedDispatchBackend,
                    ProductVectorFaultSite::KernelDispatchSelectedScoringTable,
                    *case_id,
                    true,
                )?;
                let expected_effect = ProductVectorFaultEffect::ForcedBackend {
                    requested: product_vector_backend(*requested),
                    selected: product_vector_backend(*requested),
                    kernel: zeppelin_embed::kernels::vector_fault::KernelOperationId::ScoreBit4PreparedBatch,
                    // The paired fixture scores two persisted rows across three dimensions.
                    work_items: 6,
                };
                if receipt.effect() != &expected_effect
                    || fault_result.status != vector_oracle::PrimitiveStatus::Ok
                {
                    return Err(format!(
                        "paired forced-backend receipt/public result mismatch: expected_effect={expected_effect:?} receipt={receipt:?} result={fault_result:?}"
                    ));
                }
                validated.push(ProductionFeatureReceipt::ValidatedVectorFeature(
                    receipt.clone(),
                ));
            } else {
                validated.push(ProductionFeatureReceipt::ValidatedVectorFeature(
                    child.receipt.clone(),
                ));
            }
        }
        (
            Some(vector_adapter::VectorFaultKind::CorruptCodesFactors),
            vector_adapter::VectorMutationEvidence::QuantCorruption {
                case_id,
                scheme,
                source,
                tier,
                local_row,
                field,
            },
        ) => {
            let [receipt] = receipts else {
                return Err(format!(
                    "quantization fault emitted {} production receipts, expected one",
                    receipts.len()
                ));
            };
            let (site, effect) = expected_vector_quant_effect(
                &evidence.fixture,
                *case_id,
                *scheme,
                *source,
                *tier,
                *local_row,
                *field,
            )?;
            validate_vector_receipt_header(
                receipt,
                ProductVectorOperation::Quantization,
                ProductVectorFaultKind::CorruptCodesFactors,
                site,
                *case_id,
                false,
            )?;
            if receipt.effect() != &effect {
                return Err(format!(
                    "typed vector corruption effect mismatch: expected={effect:?} observed={:?}",
                    receipt.effect()
                ));
            }
            let expected_status = match &effect {
                ProductVectorFaultEffect::CorruptedPayload {
                    field: ProductVectorQuantField::OddPadding,
                    after_bits,
                    ..
                } => vector_oracle::PrimitiveStatus::NonZeroPadding {
                    byte: u8::try_from(*after_bits).map_err(|_| {
                        format!("Bit4 padding receipt byte does not fit u8: {after_bits}")
                    })?,
                    mask: 0x0f,
                },
                ProductVectorFaultEffect::CorruptedPayload { local_row, .. } => {
                    vector_oracle::PrimitiveStatus::NonFiniteScore {
                        row: u64::from(*local_row),
                    }
                }
                other => {
                    return Err(format!(
                        "quantization corruption carried a non-corruption effect: {other:?}"
                    ));
                }
            };
            if fault_result.status != expected_status
                || !fault_result.candidates.is_empty()
                || fault_result.returned != 0
            {
                return Err(format!(
                    "quantization corruption returned the wrong public status/result exposure: expected_status={expected_status:?} observed={:?}",
                    fault_result
                ));
            }
            validated.push(ProductionFeatureReceipt::ValidatedVectorFeature(
                receipt.clone(),
            ));
        }
        (
            Some(vector_adapter::VectorFaultKind::MissingRescoreRows),
            vector_adapter::VectorMutationEvidence::MissingRescoreRows {
                case_id,
                source,
                tier,
                site,
                expected_rows,
                available_rows,
            },
        ) => {
            let [receipt] = receipts else {
                return Err(format!(
                    "missing-rescore fault emitted {} production receipts, expected one",
                    receipts.len()
                ));
            };
            let product_site = match site {
                vector_adapter::VectorRescoreMutationSite::ExactRescoreRows => {
                    ProductVectorFaultSite::ExactRescoreRows
                }
                vector_adapter::VectorRescoreMutationSite::QueryRescoreRows => {
                    ProductVectorFaultSite::QueryRescoreRows
                }
            };
            validate_vector_receipt_header(
                receipt,
                ProductVectorOperation::Rescore,
                ProductVectorFaultKind::MissingRescoreRows,
                product_site,
                *case_id,
                false,
            )?;
            let vector_oracle::PrimitiveSource::Sealed(segment) = source else {
                return Err("missing rescore fault named an active source".to_owned());
            };
            let effect = ProductVectorFaultEffect::MissingRescoreRows {
                segment: *segment,
                expected_rows: *expected_rows,
                available_rows: *available_rows,
                requested_tier: product_vector_tier(*tier)?,
            };
            if receipt.effect() != &effect {
                return Err(format!(
                    "typed missing-rescore effect mismatch: expected={effect:?} observed={:?}",
                    receipt.effect()
                ));
            }
            let expected_status = vector_oracle::PrimitiveStatus::SegmentGeometry {
                detail: format!(
                    "exact scores unavailable for segment {}: expected {expected_rows} rows, got {available_rows}",
                    evidence_hex(segment)
                ),
            };
            if fault_result.status != expected_status
                || !fault_result.candidates.is_empty()
                || fault_result.returned != 0
            {
                return Err(format!(
                    "missing-rescore fault returned the wrong public status/result exposure: expected_status={expected_status:?} observed={:?}",
                    fault_result
                ));
            }
            validated.push(ProductionFeatureReceipt::ValidatedVectorFeature(
                receipt.clone(),
            ));
        }
        (
            Some(vector_adapter::VectorFaultKind::RowCountCancellation),
            vector_adapter::VectorMutationEvidence::RowCancellation {
                case_id,
                source,
                tier,
                requested_rows,
            },
        ) => {
            let [receipt] = receipts else {
                return Err(format!(
                    "row cancellation emitted {} production receipts, expected one",
                    receipts.len()
                ));
            };
            validate_vector_receipt_header(
                receipt,
                ProductVectorOperation::RowIdentity,
                ProductVectorFaultKind::RowCountCancellation,
                ProductVectorFaultSite::ScoredVectorRow,
                *case_id,
                false,
            )?;
            let ProductVectorFaultEffect::CancelledAfterRows {
                source: observed_source,
                requested_tier,
                local_row,
                requested_rows: observed_requested,
                observed_rows,
            } = receipt.effect()
            else {
                return Err(format!(
                    "typed cancellation receipt carried another effect: {:?}",
                    receipt.effect()
                ));
            };
            if *observed_source != product_vector_source(*source)
                || *requested_tier != product_vector_tier(*tier)?
                || *observed_requested != *requested_rows
                || *observed_rows != *requested_rows
                || usize::try_from(*local_row)
                    .map_or(true, |row| row >= evidence.fixture.documents.len().max(1))
            {
                return Err(format!(
                    "typed cancellation effect differs from the planned source/tier/count: {:?}",
                    receipt.effect()
                ));
            }
            if fault_result.status != (vector_oracle::PrimitiveStatus::Cancelled { partial: false })
            {
                return Err(format!(
                    "vector cancellation returned the wrong public status: {:?}",
                    fault_result.status
                ));
            }
            validated.push(ProductionFeatureReceipt::ValidatedVectorFeature(
                receipt.clone(),
            ));
        }
        (
            Some(vector_adapter::VectorFaultKind::AllocationDenial),
            vector_adapter::VectorMutationEvidence::AllocationDenial {
                case_id,
                component,
                items,
                bytes,
            },
        ) => {
            let [receipt] = receipts else {
                return Err(format!(
                    "allocation denial emitted {} production receipts, expected one",
                    receipts.len()
                ));
            };
            validate_vector_receipt_header(
                receipt,
                ProductVectorOperation::RowIdentity,
                ProductVectorFaultKind::AllocationDenial,
                ProductVectorFaultSite::SearchGlobalCandidates,
                *case_id,
                false,
            )?;
            let expected_effect = ProductVectorFaultEffect::AllocationDenied {
                component: ProductVectorAllocationSite::SearchGlobalCandidates,
                requested_tier: ProductVectorSearchTier::Exact,
                items: *items,
                bytes: *bytes,
            };
            if receipt.effect() != &expected_effect {
                return Err(format!(
                    "typed allocation-denial effect mismatch: expected={expected_effect:?} observed={:?}",
                    receipt.effect()
                ));
            }
            let expected_status = vector_oracle::PrimitiveStatus::AllocationFailed {
                component: (*component).to_owned(),
                needed: *bytes,
            };
            if fault_result.status != expected_status {
                return Err(format!(
                    "allocation denial returned the wrong public status: expected {expected_status:?}, observed {:?}",
                    fault_result.status
                ));
            }
            validated.push(ProductionFeatureReceipt::ValidatedVectorFeature(
                receipt.clone(),
            ));
        }
        _ => {
            return Err(format!(
                "vector fault/mutation mismatch: fault={:?} mutation={:?}",
                evidence.fault, mutation
            ));
        }
    }
    if evidence.fault.is_some() && fault_result.generation != clean_result.generation {
        return Err(format!(
            "vector fault changed public generation: clean={} fault={}",
            clean_result.generation, fault_result.generation
        ));
    }
    Ok(validated)
}

#[cfg(test)]
mod vector_receipt_credit_tests {
    use super::*;

    #[test]
    fn vector_generic_fault_that_blocks_the_feature_site_cannot_earn_feature_credit() {
        let evidence = vector_adapter::run_vector_operation_with_context(
            vector_adapter::VectorOperationKind::Quantization,
            0x5646_5041,
            Some(vector_adapter::VectorFaultKind::CorruptCodesFactors),
            vector_adapter::VectorExecutionContext {
                program_op_index: 17,
                generic_fault: Some(vector_adapter::VectorGenericFaultSchedule {
                    id: "vector-generic-blocks-feature".to_owned(),
                    site: vector_adapter::VectorGenericFaultSite::Append,
                    mode: vector_adapter::VectorGenericFaultMode::Eio,
                    nth_match: 1,
                    path_contains: Some("wal.ze".to_owned()),
                }),
            },
        )
        .expect("observe paired generic and vector faults");

        let Err(error) = validate_vector_receipts(&evidence) else {
            panic!("a generic fault that blocked the vector site earned feature credit");
        };
        assert!(error.contains("blocked the vector feature site"), "{error}");
    }

    #[test]
    fn vector_generic_feature_credit_uses_the_exact_paired_mutation() {
        let mut evidence = vector_adapter::run_vector_operation_with_context(
            vector_adapter::VectorOperationKind::Quantization,
            26,
            Some(vector_adapter::VectorFaultKind::CorruptCodesFactors),
            vector_adapter::VectorExecutionContext {
                program_op_index: 18,
                generic_fault: Some(vector_adapter::VectorGenericFaultSchedule {
                    id: "vector-generic-read-latency".to_owned(),
                    site: vector_adapter::VectorGenericFaultSite::Read,
                    mode: vector_adapter::VectorGenericFaultMode::Latency,
                    nth_match: 1,
                    path_contains: Some("manifest.ze".to_owned()),
                }),
            },
        )
        .expect("observe non-blocking paired vector fault");

        let receipts = validate_vector_receipts(&evidence)
            .expect("exact paired feature mutation should earn one receipt");
        assert_eq!(receipts.len(), 1);

        evidence
            .generic_fault
            .as_mut()
            .expect("paired generic evidence")
            .feature_mutation = vector_adapter::VectorMutationEvidence::None;
        let Err(error) = validate_vector_receipts(&evidence) else {
            panic!("a receipt detached from its paired mutation earned feature credit");
        };
        assert!(error.contains("fault/mutation mismatch"), "{error}");
    }

    #[test]
    fn vector_generic_forced_backend_credit_uses_the_paired_receipt() {
        let evidence = vector_adapter::run_vector_operation_with_context(
            vector_adapter::VectorOperationKind::KernelParity,
            0,
            Some(vector_adapter::VectorFaultKind::ForcedDispatchBackend),
            vector_adapter::VectorExecutionContext {
                program_op_index: 19,
                generic_fault: Some(vector_adapter::VectorGenericFaultSchedule {
                    id: "vector-generic-kernel-read-latency".to_owned(),
                    site: vector_adapter::VectorGenericFaultSite::Read,
                    mode: vector_adapter::VectorGenericFaultMode::Latency,
                    nth_match: 1,
                    path_contains: Some("manifest.ze".to_owned()),
                }),
            },
        )
        .expect("observe paired forced backend fault");

        let generic = evidence.generic_fault.as_ref().expect("generic evidence");
        let [paired_receipt] = generic.fault.feature_receipts.as_slice() else {
            panic!("paired forced backend leg did not emit one receipt");
        };
        let validated_receipts = validate_vector_receipts(&evidence)
            .expect("paired forced backend receipt should earn credit");
        let [ProductionFeatureReceipt::ValidatedVectorFeature(validated)] =
            validated_receipts.as_slice()
        else {
            panic!("paired forced backend credit returned another receipt shape");
        };
        assert_eq!(validated, paired_receipt);
        assert_ne!(
            validated,
            &evidence.forced_child.expect("child evidence").receipt
        );
    }

    #[test]
    fn vector_generic_evidence_serialization_binds_the_fault_pair() {
        let evidence = vector_adapter::run_vector_operation_with_context(
            vector_adapter::VectorOperationKind::Quantization,
            26,
            Some(vector_adapter::VectorFaultKind::CorruptCodesFactors),
            vector_adapter::VectorExecutionContext {
                program_op_index: 18,
                generic_fault: Some(vector_adapter::VectorGenericFaultSchedule {
                    id: "vector-generic-read-latency".to_owned(),
                    site: vector_adapter::VectorGenericFaultSite::Read,
                    mode: vector_adapter::VectorGenericFaultMode::Latency,
                    nth_match: 1,
                    path_contains: Some("manifest.ze".to_owned()),
                }),
            },
        )
        .expect("observe serializable vector fault pair");
        let serialized = vector_generic_fault_json(evidence.generic_fault.as_ref());
        let value: zeppelin_embed_bench::harness_json::Value =
            zeppelin_embed_bench::harness_json::from_str(&serialized)
                .expect("parse vector generic evidence JSON");
        assert_eq!(value["schedule"]["id"], "vector-generic-read-latency");
        assert_eq!(value["program_op_index"].as_u64(), Some(18));
        assert_eq!(
            value["clean"]["feature_receipts"].as_array().map(Vec::len),
            Some(0)
        );
        assert_eq!(
            value["fault"]["feature_receipts"].as_array().map(Vec::len),
            Some(1)
        );
        assert_eq!(value["feature_mutation"]["kind"], "quant-corruption");
        assert_eq!(value["isolated_directories"].as_bool(), Some(true));
        assert_eq!(value["isolated_runtimes"].as_bool(), Some(true));
    }

    #[test]
    fn vector_control_credit_requires_isolated_byte_identical_fixture_directories() {
        let mut evidence = vector_adapter::run_vector_operation(
            vector_adapter::VectorOperationKind::Quantization,
            0,
            Some(vector_adapter::VectorFaultKind::CorruptCodesFactors),
        )
        .expect("observe isolated vector control fixtures");
        evidence.control.isolated_directories = false;
        let Err(error) = validate_vector_receipts(&evidence) else {
            panic!("one-directory vector control earned fault credit");
        };
        assert!(error.contains("isolated"), "{error}");

        evidence.control.isolated_directories = true;
        evidence.control.fault_initial_directory.digest ^= 1;
        let Err(error) = validate_vector_receipts(&evidence) else {
            panic!("non-identical vector fixture directories earned fault credit");
        };
        assert!(error.contains("byte-identical"), "{error}");
    }

    #[test]
    fn quant_corruption_cannot_earn_fault_credit_with_a_success_status() {
        let mut evidence = vector_adapter::run_vector_operation(
            vector_adapter::VectorOperationKind::Quantization,
            0,
            Some(vector_adapter::VectorFaultKind::CorruptCodesFactors),
        )
        .expect("observe real quantization corruption receipt");
        evidence.control.fault.status = vector_oracle::PrimitiveStatus::Ok;

        let Err(error) = validate_vector_receipts(&evidence) else {
            panic!("quant corruption receipt earned credit despite public success");
        };
        assert!(error.contains("wrong public status"), "{error}");
    }

    #[test]
    fn missing_rescore_rows_cannot_earn_fault_credit_with_a_success_status() {
        let mut evidence = vector_adapter::run_vector_operation(
            vector_adapter::VectorOperationKind::Rescore,
            0,
            Some(vector_adapter::VectorFaultKind::MissingRescoreRows),
        )
        .expect("observe real missing-rescore receipt");
        evidence.control.fault.status = vector_oracle::PrimitiveStatus::Ok;

        let Err(error) = validate_vector_receipts(&evidence) else {
            panic!("missing-rescore receipt earned credit despite public success");
        };
        assert!(error.contains("wrong public status"), "{error}");
    }
}

fn vector_json_string_array(values: impl IntoIterator<Item = String>) -> String {
    values
        .into_iter()
        .map(|value| format!("\"{}\"", json_escape(&value)))
        .collect::<Vec<_>>()
        .join(",")
}

fn vector_json_values<T>(values: &[T], render: impl Fn(&T) -> String) -> String {
    values.iter().map(render).collect::<Vec<_>>().join(",")
}

fn vector_json_numbers<T: std::fmt::Display>(values: &[T]) -> String {
    vector_json_values(values, ToString::to_string)
}

fn vector_f32_bits_json(values: &[vector_oracle::F32]) -> String {
    vector_json_values(values, |value| value.0.to_string())
}

fn vector_document_json(document: vector_oracle::PrimitiveDocument) -> String {
    format!(
        "{{\"doc_id_be\":\"{}\",\"revision\":{}}}",
        evidence_hex(&document.doc_id_be),
        document.revision
    )
}

fn vector_source_json(source: vector_oracle::PrimitiveSource) -> String {
    match source {
        vector_oracle::PrimitiveSource::Active => "{\"kind\":\"active\"}".to_owned(),
        vector_oracle::PrimitiveSource::Sealed(segment) => format!(
            "{{\"kind\":\"sealed\",\"segment\":\"{}\"}}",
            evidence_hex(&segment)
        ),
    }
}

fn vector_row_json(row: vector_oracle::PrimitiveRow) -> String {
    format!(
        "{{\"source\":{},\"local_row\":{}}}",
        vector_source_json(row.source),
        row.local_row
    )
}

fn vector_status_json(status: &vector_oracle::PrimitiveStatus) -> String {
    match status {
        vector_oracle::PrimitiveStatus::Ok => "{\"kind\":\"ok\"}".to_owned(),
        vector_oracle::PrimitiveStatus::EmptyVector => "{\"kind\":\"empty-vector\"}".to_owned(),
        vector_oracle::PrimitiveStatus::DimensionTooLarge { actual, maximum } => format!(
            "{{\"kind\":\"dimension-too-large\",\"actual\":{actual},\"maximum\":{maximum}}}"
        ),
        vector_oracle::PrimitiveStatus::NonFinite { index } => {
            format!("{{\"kind\":\"non-finite\",\"index\":{index}}}")
        }
        vector_oracle::PrimitiveStatus::OutputLength { expected, actual } => {
            format!("{{\"kind\":\"output-length\",\"expected\":{expected},\"actual\":{actual}}}")
        }
        vector_oracle::PrimitiveStatus::CodeLength { expected, actual } => {
            format!("{{\"kind\":\"code-length\",\"expected\":{expected},\"actual\":{actual}}}")
        }
        vector_oracle::PrimitiveStatus::NonZeroPadding { byte, mask } => {
            format!("{{\"kind\":\"non-zero-padding\",\"byte\":{byte},\"mask\":{mask}}}")
        }
        vector_oracle::PrimitiveStatus::CandidateRowCount { expected, actual } => format!(
            "{{\"kind\":\"candidate-row-count\",\"expected\":{expected},\"actual\":{actual}}}"
        ),
        vector_oracle::PrimitiveStatus::CandidateRowOutOfRange { row, rows } => {
            format!("{{\"kind\":\"candidate-row-out-of-range\",\"row\":{row},\"rows\":{rows}}}")
        }
        vector_oracle::PrimitiveStatus::NonFiniteScore { row } => {
            format!("{{\"kind\":\"non-finite-score\",\"row\":{row}}}")
        }
        vector_oracle::PrimitiveStatus::Cancelled { partial } => {
            format!("{{\"kind\":\"cancelled\",\"partial\":{partial}}}")
        }
        vector_oracle::PrimitiveStatus::AllocationFailed { component, needed } => format!(
            "{{\"kind\":\"allocation-failed\",\"component\":\"{}\",\"needed\":{needed}}}",
            json_escape(component)
        ),
        vector_oracle::PrimitiveStatus::SegmentGeometry { detail } => format!(
            "{{\"kind\":\"segment-geometry\",\"detail\":\"{}\"}}",
            json_escape(detail)
        ),
    }
}

fn vector_schedule_key(step: vector_oracle::PublicStoreStep) -> &'static str {
    match step {
        vector_oracle::PublicStoreStep::IngestAccepted => "ingest-accepted",
        vector_oracle::PublicStoreStep::IngestRejected => "ingest-rejected",
        vector_oracle::PublicStoreStep::Seal => "seal",
        vector_oracle::PublicStoreStep::PublishPreparedSegment => "publish-prepared-segment",
        vector_oracle::PublicStoreStep::DeleteAccepted => "delete-accepted",
        vector_oracle::PublicStoreStep::Reopen => "reopen",
        vector_oracle::PublicStoreStep::Search => "search",
    }
}

fn vector_schedule_json(steps: &[vector_oracle::PublicStoreStep]) -> String {
    vector_json_string_array(
        steps
            .iter()
            .map(|step| vector_schedule_key(*step).to_owned()),
    )
}

fn vector_kernel_key(kernel: vector_oracle::KernelId) -> &'static str {
    match kernel {
        vector_oracle::KernelId::DotI8 => "dot-i8",
        vector_oracle::KernelId::HammingU1 => "hamming-u1",
        vector_oracle::KernelId::DotF32 => "dot-f32",
        vector_oracle::KernelId::DotF16 => "dot-f16",
        vector_oracle::KernelId::DotI8Batch => "dot-i8-batch",
        vector_oracle::KernelId::HammingU1Batch => "hamming-u1-batch",
        vector_oracle::KernelId::DotBit4 => "dot-bit4",
        vector_oracle::KernelId::DotBit4Prepared => "dot-bit4-prepared",
        vector_oracle::KernelId::DotBit4Batch => "dot-bit4-batch",
        vector_oracle::KernelId::ScoreBit4PreparedBatch => "score-bit4-prepared-batch",
        vector_oracle::KernelId::ScoreBit4Ptrs => "score-bit4-ptrs",
    }
}

fn vector_kernel_input_json(input: &vector_oracle::KernelInput) -> String {
    let factors = vector_json_values(&input.bit4_factors, |factor| {
        format!("[{},{},{}]", factor[0].0, factor[1].0, factor[2].0)
    });
    format!(
        "{{\"case_id\":{},\"backend\":\"{}\",\"selected_for_store\":{},\"work_items\":{},\"kernel\":\"{}\",\"dimension\":{},\"input_offset\":{},\"signed_a\":[{}],\"signed_b\":[{}],\"bytes_a\":[{}],\"bytes_b\":[{}],\"f32_a_bits\":[{}],\"f32_b_bits\":[{}],\"f16_a_bits\":[{}],\"f16_b_bits\":[{}],\"row_bytes\":{},\"batch_rows\":{},\"pointer_order\":[{}],\"query_sum\":{},\"query_scale_half_bits\":{},\"bit4_factor_bits\":[{}]}}",
        input.case_id,
        vector_backend_key(input.backend),
        input.selected_for_store,
        input.work_items,
        vector_kernel_key(input.kernel),
        input.dimension,
        input.input_offset(),
        vector_json_numbers(&input.signed_a),
        vector_json_numbers(&input.signed_b),
        vector_json_numbers(&input.bytes_a),
        vector_json_numbers(&input.bytes_b),
        vector_f32_bits_json(&input.f32_a),
        vector_f32_bits_json(&input.f32_b),
        vector_json_numbers(&input.f16_a),
        vector_json_numbers(&input.f16_b),
        input.row_bytes,
        input.batch_rows,
        vector_json_numbers(&input.pointer_order),
        input.query_sum,
        input.query_scale_half.0,
        factors,
    )
}

fn vector_kernel_value_json(value: &vector_oracle::KernelValue) -> String {
    match value {
        vector_oracle::KernelValue::S32(value) => {
            format!("{{\"kind\":\"s32\",\"value\":{value}}}")
        }
        vector_oracle::KernelValue::U32(value) => {
            format!("{{\"kind\":\"u32\",\"value\":{value}}}")
        }
        vector_oracle::KernelValue::F32(value) => {
            format!("{{\"kind\":\"f32\",\"bits\":{}}}", value.0)
        }
        vector_oracle::KernelValue::S32s(values) => format!(
            "{{\"kind\":\"s32s\",\"values\":[{}]}}",
            vector_json_numbers(values)
        ),
        vector_oracle::KernelValue::U32s(values) => format!(
            "{{\"kind\":\"u32s\",\"values\":[{}]}}",
            vector_json_numbers(values)
        ),
        vector_oracle::KernelValue::F32s(values) => format!(
            "{{\"kind\":\"f32s\",\"bits\":[{}]}}",
            vector_f32_bits_json(values)
        ),
    }
}

fn vector_optional_f64_json(value: Option<vector_oracle::F64>) -> String {
    value.map_or_else(|| "null".to_owned(), |value| value.0.to_string())
}

fn vector_i24_expected_json(expected: &vector_oracle::I24Expected) -> String {
    format!(
        "{{\"case_id\":{},\"backend\":\"{}\",\"kernel\":\"{}\",\"selected_for_store\":{},\"work_items\":{},\"exact\":{},\"reference_bits\":{},\"magnitude_bits\":{},\"tolerance_bits\":{}}}",
        expected.case_id,
        vector_backend_key(expected.backend),
        vector_kernel_key(expected.kernel),
        expected.selected_for_store,
        expected.work_items,
        vector_kernel_value_json(&expected.exact),
        vector_optional_f64_json(expected.reference),
        vector_optional_f64_json(expected.magnitude),
        vector_optional_f64_json(expected.tolerance),
    )
}

fn vector_i24_observed_json(observed: &vector_oracle::I24Observed) -> String {
    format!(
        "{{\"case_id\":{},\"backend\":\"{}\",\"kernel\":\"{}\",\"value\":{},\"selected_for_store\":{},\"work_items\":{}}}",
        observed.case_id,
        vector_backend_key(observed.backend),
        vector_kernel_key(observed.kernel),
        vector_kernel_value_json(&observed.value),
        observed.selected_for_store,
        observed.work_items,
    )
}

fn vector_quant_scheme_key(scheme: vector_oracle::QuantScheme) -> &'static str {
    match scheme {
        vector_oracle::QuantScheme::Bit4 => "bit4",
        vector_oracle::QuantScheme::Int8 => "int8",
    }
}

fn vector_quant_input_json(input: &vector_oracle::QuantInput) -> String {
    let document = input
        .store
        .document
        .map_or_else(|| "null".to_owned(), vector_document_json);
    format!(
        "{{\"case_id\":{},\"scheme\":\"{}\",\"row_bits\":[{}],\"query_bits\":[{}],\"query_seed\":{},\"output_len\":{},\"code_len\":{},\"sentinel\":{},\"store\":{{\"generation_before\":{},\"schedule\":[{}],\"document\":{},\"document_visible\":{}}}}}",
        input.case_id,
        vector_quant_scheme_key(input.scheme),
        vector_f32_bits_json(&input.row),
        vector_f32_bits_json(&input.query),
        input.query_seed,
        input.output_len,
        input.code_len,
        input.sentinel,
        input.store.generation_before,
        vector_schedule_json(&input.store.schedule),
        document,
        input.store.document_visible,
    )
}

fn vector_quant_success_json(success: &vector_oracle::QuantSuccess) -> String {
    format!(
        "{{\"code_bytes\":[{}],\"factor_bits\":[{}],\"query_code_bytes\":[{}],\"query_code_sum\":{},\"query_scale_bits\":{},\"reconstruction_bits\":[{}],\"estimate_bits\":{}}}",
        vector_json_numbers(&success.code_bytes),
        vector_f32_bits_json(&success.factor_bits),
        vector_json_numbers(&success.query_code_bytes),
        success.query_code_sum,
        success.query_scale.0,
        vector_f32_bits_json(&success.reconstruction),
        success.estimate.0,
    )
}

fn vector_quant_result_json(result: &vector_oracle::QuantResult) -> String {
    let success = result
        .success
        .as_ref()
        .map_or_else(|| "null".to_owned(), vector_quant_success_json);
    format!(
        "{{\"status\":{},\"output_after\":[{}],\"success\":{success}}}",
        vector_status_json(&result.status),
        vector_json_numbers(&result.output_after),
    )
}

fn vector_quant_store_facts_json(facts: &vector_oracle::QuantStoreFacts) -> String {
    format!(
        "{{\"ingest_status\":{},\"scan_status\":{},\"generation_before\":{},\"generation_after\":{},\"document_visible\":{},\"persisted_code_bytes\":[{}],\"persisted_factor_bits\":[{}]}}",
        vector_status_json(&facts.ingest_status),
        vector_status_json(&facts.scan_status),
        facts.generation_before,
        facts.generation_after,
        facts.document_visible,
        vector_json_numbers(&facts.persisted_code_bytes),
        vector_f32_bits_json(&facts.persisted_factor_bits),
    )
}

fn vector_i25_observed_json(observed: &vector_oracle::I25Observed) -> String {
    format!(
        "{{\"case_id\":{},\"result\":{},\"store\":{}}}",
        observed.case_id,
        vector_quant_result_json(&observed.result),
        vector_quant_store_facts_json(&observed.store),
    )
}

fn vector_i25_expected_json(expected: &vector_oracle::I25Expected) -> String {
    format!(
        "{{\"input\":{},\"result\":{},\"l2_error_bits\":{},\"l2_bound_bits\":{},\"estimate_reference_bits\":{},\"estimate_error_bits\":{},\"estimate_bound_bits\":{}}}",
        vector_quant_input_json(&expected.input),
        vector_quant_result_json(&expected.result),
        expected.l2_error.0,
        expected.l2_bound.0,
        expected.estimate_reference.0,
        expected.estimate_error.0,
        expected.estimate_bound.0,
    )
}

fn vector_rescore_metric_key(metric: vector_oracle::RescoreMetric) -> &'static str {
    match metric {
        vector_oracle::RescoreMetric::InnerProduct => "inner-product",
        vector_oracle::RescoreMetric::SquaredL2 => "squared-l2",
    }
}

fn vector_candidate_mode_json(mode: &vector_oracle::CandidateMode) -> String {
    match mode {
        vector_oracle::CandidateMode::Dense { coarse, oversample } => format!(
            "{{\"kind\":\"dense\",\"coarse_bits\":[{}],\"oversample\":{oversample}}}",
            vector_f32_bits_json(coarse)
        ),
        vector_oracle::CandidateMode::Retained { rows, coarse } => format!(
            "{{\"kind\":\"retained\",\"rows\":[{}],\"coarse_bits\":[{}]}}",
            vector_json_numbers(rows),
            vector_f32_bits_json(coarse),
        ),
    }
}

fn vector_optional_document_json(document: &Option<vector_oracle::PrimitiveDocument>) -> String {
    document.map_or_else(|| "null".to_owned(), vector_document_json)
}

fn vector_rescore_input_json(input: &vector_oracle::RescoreInput) -> String {
    let store = input.store.as_ref().map_or_else(
        || "null".to_owned(),
        |store| {
            let documents = vector_json_values(&store.documents_by_row, vector_optional_document_json);
            format!(
                "{{\"source\":{},\"documents_by_row\":[{documents}],\"tier\":{},\"exact_rescore\":{}}}",
                vector_source_json(store.source),
                store.tier,
                store.exact_rescore,
            )
        },
    );
    format!(
        "{{\"case_id\":{},\"metric\":\"{}\",\"query_bits\":[{}],\"rows_row_major_bits\":[{}],\"dimension\":{},\"k\":{},\"candidates\":{},\"coarse_rows_touched\":{},\"coarse_bytes_per_row\":{},\"store\":{store}}}",
        input.case_id,
        vector_rescore_metric_key(input.metric),
        vector_f32_bits_json(&input.query),
        vector_f32_bits_json(&input.rows_row_major),
        input.dimension,
        input.k,
        vector_candidate_mode_json(&input.candidates),
        input.coarse_rows_touched,
        input.coarse_bytes_per_row,
    )
}

fn vector_rescore_hit_json(hit: &vector_oracle::PrimitiveRescoreHit) -> String {
    format!("{{\"row\":{},\"score_bits\":{}}}", hit.row, hit.score.0)
}

fn vector_store_rescore_hit_json(hit: &vector_oracle::StoreRescoreHit) -> String {
    format!(
        "{{\"row\":{},\"document\":{},\"score_bits\":{},\"exact_score\":{}}}",
        vector_row_json(hit.row),
        vector_optional_document_json(&hit.document),
        hit.score.0,
        hit.exact_score,
    )
}

fn vector_i26_observed_json(observed: &vector_oracle::I26Observed) -> String {
    format!(
        "{{\"case_id\":{},\"primitive_status\":{},\"primitive_hits\":[{}],\"primitive_counts\":[{}],\"tier\":{},\"store_hits\":[{}],\"exact_rescore\":{}}}",
        observed.case_id,
        vector_status_json(&observed.primitive_status),
        vector_json_values(&observed.primitive_hits, vector_rescore_hit_json),
        vector_json_numbers(&observed.primitive_counts),
        observed.tier,
        vector_json_values(&observed.store_hits, vector_store_rescore_hit_json),
        observed.exact_rescore,
    )
}

fn vector_i26_expected_json(expected: &vector_oracle::I26Expected) -> String {
    let store_tier = expected
        .store_tier
        .map_or_else(|| "null".to_owned(), |tier| tier.to_string());
    format!(
        "{{\"case_id\":{},\"metric\":\"{}\",\"status\":{},\"hits\":[{}],\"score_tolerance_bits\":[{}],\"candidates_rescored\":{},\"coarse_bytes\":{},\"rescore_bytes\":{},\"total_bytes\":{},\"store_hits\":[{}],\"store_tier\":{store_tier},\"store_exact_rescore\":{}}}",
        expected.case_id,
        vector_rescore_metric_key(expected.metric),
        vector_status_json(&expected.status),
        vector_json_values(&expected.hits, vector_rescore_hit_json),
        vector_json_numbers(
            &expected
                .score_tolerances
                .iter()
                .map(|value| value.0)
                .collect::<Vec<_>>()
        ),
        expected.candidates_rescored,
        expected.coarse_bytes,
        expected.rescore_bytes,
        expected.total_bytes,
        vector_json_values(&expected.store_hits, vector_store_rescore_hit_json),
        expected.store_exact_rescore,
    )
}

fn vector_identity_mutation_json(mutation: &vector_oracle::IdentityMutation) -> String {
    match mutation {
        vector_oracle::IdentityMutation::Ingest(document) => format!(
            "{{\"kind\":\"ingest\",\"document\":{}}}",
            vector_document_json(*document)
        ),
        vector_oracle::IdentityMutation::Seal(segment) => format!(
            "{{\"kind\":\"seal\",\"segment\":\"{}\"}}",
            evidence_hex(segment)
        ),
        vector_oracle::IdentityMutation::Reopen => "{\"kind\":\"reopen\"}".to_owned(),
        vector_oracle::IdentityMutation::Replace(document) => format!(
            "{{\"kind\":\"replace\",\"document\":{}}}",
            vector_document_json(*document)
        ),
        vector_oracle::IdentityMutation::Delete {
            doc_id_be,
            revision,
        } => format!(
            "{{\"kind\":\"delete\",\"doc_id_be\":\"{}\",\"revision\":{revision}}}",
            evidence_hex(doc_id_be)
        ),
    }
}

fn vector_identity_input_json(input: &vector_oracle::IdentityInput) -> String {
    format!(
        "{{\"case_id\":{},\"mutations\":[{}],\"query_bits\":[{}],\"k\":{},\"tier\":{},\"observation_phase\":{},\"public_schedule\":[{}]}}",
        input.case_id,
        vector_json_values(&input.mutations, vector_identity_mutation_json),
        vector_f32_bits_json(&input.query),
        input.k,
        input.tier,
        input.observation_phase,
        vector_schedule_json(&input.public_schedule),
    )
}

fn vector_identity_observed_row_json(row: &vector_oracle::IdentityObservedRow) -> String {
    format!(
        "{{\"row\":{},\"document\":{},\"score_bits\":{}}}",
        vector_row_json(row.row),
        vector_optional_document_json(&row.document),
        row.score.0,
    )
}

fn vector_i27_observed_json(observed: &vector_oracle::I27Observed) -> String {
    format!(
        "{{\"case_id\":{},\"rows\":[{}],\"generation\":{},\"phase\":{},\"tier\":{},\"control_rows\":[{}],\"control_generation\":{},\"retry_rows\":[{}],\"retry_generation\":{},\"fault_status\":{},\"retry_status\":{}}}",
        observed.case_id,
        vector_json_values(&observed.rows, vector_identity_observed_row_json),
        observed.generation,
        observed.phase,
        observed.tier,
        vector_json_values(&observed.control_rows, vector_identity_observed_row_json),
        observed.control_generation,
        vector_json_values(&observed.retry_rows, vector_identity_observed_row_json),
        observed.retry_generation,
        vector_status_json(&observed.fault_status),
        vector_status_json(&observed.retry_status),
    )
}

fn vector_i27_expected_row_json(row: &vector_oracle::IdentityExpectedRow) -> String {
    format!(
        "{{\"document\":{},\"row\":{},\"phase\":{}}}",
        vector_document_json(row.document),
        vector_row_json(row.row),
        row.phase,
    )
}

fn vector_i27_expected_json(expected: &vector_oracle::I27Expected) -> String {
    format!(
        "{{\"case_id\":{},\"visible\":[{}],\"forbidden\":[{}],\"generation\":{},\"phase\":{},\"tier\":{}}}",
        expected.case_id,
        vector_json_values(&expected.visible, vector_i27_expected_row_json),
        vector_json_values(&expected.forbidden, |document| vector_document_json(
            *document
        )),
        expected.generation,
        expected.phase,
        expected.tier,
    )
}

fn vector_store_result_json(result: &vector_adapter::VectorStoreResultFact) -> String {
    format!(
        "{{\"status\":{},\"generation\":{},\"candidates\":[{}],\"dims_touched\":{},\"bytes_read\":{},\"exact_rescore\":{},\"approximate\":{},\"returned\":{}}}",
        vector_status_json(&result.status),
        result.generation,
        vector_json_values(&result.candidates, vector_identity_observed_row_json),
        result.dims_touched,
        result.bytes_read,
        result.exact_rescore,
        result.approximate,
        result.returned,
    )
}

fn vector_fixture_directory_json(
    directory: &vector_adapter::VectorFixtureDirectoryEvidence,
) -> String {
    let files = directory
        .files
        .iter()
        .map(|file| {
            format!(
                "{{\"relative_path\":\"{}\",\"byte_length\":{},\"digest\":{}}}",
                json_escape(&file.relative_path),
                file.byte_length,
                file.digest,
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("{{\"digest\":{},\"files\":[{files}]}}", directory.digest,)
}

fn vector_control_json(control: &vector_adapter::VectorControlEvidence) -> String {
    format!(
        "{{\"namespace\":\"{}\",\"operation\":\"{}\",\"seed\":{},\"clean\":{},\"fault\":{},\"retry\":{},\"clean_initial_directory\":{},\"fault_initial_directory\":{},\"isolated_directories\":{}}}",
        json_escape(control.namespace),
        control.operation.key(),
        control.seed,
        vector_store_result_json(&control.clean),
        vector_store_result_json(&control.fault),
        vector_store_result_json(&control.retry),
        vector_fixture_directory_json(&control.clean_initial_directory),
        vector_fixture_directory_json(&control.fault_initial_directory),
        control.isolated_directories,
    )
}

fn vector_mutation_json(mutation: &vector_adapter::VectorMutationEvidence) -> String {
    match mutation {
        vector_adapter::VectorMutationEvidence::None => "{\"kind\":\"none\"}".to_owned(),
        vector_adapter::VectorMutationEvidence::ForcedDispatch { case_id, requested } => format!(
            "{{\"kind\":\"forced-dispatch\",\"case_id\":{case_id},\"requested\":\"{}\"}}",
            vector_backend_key(*requested)
        ),
        vector_adapter::VectorMutationEvidence::QuantCorruption {
            case_id,
            scheme,
            source,
            tier,
            local_row,
            field,
        } => format!(
            "{{\"kind\":\"quant-corruption\",\"case_id\":{case_id},\"scheme\":\"{}\",\"source\":{},\"tier\":{tier},\"local_row\":{local_row},\"field\":\"{:?}\"}}",
            vector_quant_scheme_key(*scheme),
            vector_source_json(*source),
            field,
        ),
        vector_adapter::VectorMutationEvidence::MissingRescoreRows {
            case_id,
            source,
            tier,
            site,
            expected_rows,
            available_rows,
        } => format!(
            "{{\"kind\":\"missing-rescore-rows\",\"case_id\":{case_id},\"source\":{},\"tier\":{tier},\"site\":\"{:?}\",\"expected_rows\":{expected_rows},\"available_rows\":{available_rows}}}",
            vector_source_json(*source),
            site,
        ),
        vector_adapter::VectorMutationEvidence::RowCancellation {
            case_id,
            source,
            tier,
            requested_rows,
        } => format!(
            "{{\"kind\":\"row-cancellation\",\"case_id\":{case_id},\"source\":{},\"tier\":{tier},\"requested_rows\":{requested_rows}}}",
            vector_source_json(*source),
        ),
        vector_adapter::VectorMutationEvidence::AllocationDenial {
            case_id,
            component,
            items,
            bytes,
        } => format!(
            "{{\"kind\":\"allocation-denial\",\"case_id\":{case_id},\"component\":\"{}\",\"items\":{items},\"bytes\":{bytes}}}",
            json_escape(component),
        ),
    }
}

fn vector_generic_fault_site_key(site: vector_adapter::VectorGenericFaultSite) -> &'static str {
    match site {
        vector_adapter::VectorGenericFaultSite::Open => "open",
        vector_adapter::VectorGenericFaultSite::Read => "read",
        vector_adapter::VectorGenericFaultSite::ReadRange => "read-range",
        vector_adapter::VectorGenericFaultSite::Write => "write",
        vector_adapter::VectorGenericFaultSite::Append => "append",
        vector_adapter::VectorGenericFaultSite::Sync => "sync",
        vector_adapter::VectorGenericFaultSite::Rename => "rename",
        vector_adapter::VectorGenericFaultSite::List => "list",
        vector_adapter::VectorGenericFaultSite::Delete => "delete",
    }
}

fn vector_generic_fault_mode_key(mode: vector_adapter::VectorGenericFaultMode) -> &'static str {
    match mode {
        vector_adapter::VectorGenericFaultMode::Eio => "eio",
        vector_adapter::VectorGenericFaultMode::Eacces => "eacces",
        vector_adapter::VectorGenericFaultMode::Enospc => "enospc",
        vector_adapter::VectorGenericFaultMode::BitFlip => "bit-flip",
        vector_adapter::VectorGenericFaultMode::TornWrite => "torn-write",
        vector_adapter::VectorGenericFaultMode::Truncate => "truncate",
        vector_adapter::VectorGenericFaultMode::WrongObject => "wrong-object",
        vector_adapter::VectorGenericFaultMode::MisdirectedWrite => "misdirected-write",
        vector_adapter::VectorGenericFaultMode::ZeroFill => "zero-fill",
        vector_adapter::VectorGenericFaultMode::Latency => "latency",
        vector_adapter::VectorGenericFaultMode::SilentDrop => "silent-drop",
        vector_adapter::VectorGenericFaultMode::PostCommitError => "post-commit-error",
    }
}

fn vector_generic_fault_stage_key(stage: vector_adapter::VectorGenericFaultStage) -> &'static str {
    match stage {
        vector_adapter::VectorGenericFaultStage::Open => "open",
        vector_adapter::VectorGenericFaultStage::Ingest => "ingest",
        vector_adapter::VectorGenericFaultStage::Seal => "seal",
        vector_adapter::VectorGenericFaultStage::Close => "close",
        vector_adapter::VectorGenericFaultStage::Reopen => "reopen",
        vector_adapter::VectorGenericFaultStage::Query => "query",
    }
}

fn vector_store_error_kind_key(kind: zeppelin_embed::lifecycle::StoreErrorKind) -> &'static str {
    use zeppelin_embed::lifecycle::StoreErrorKind;
    match kind {
        StoreErrorKind::Io => "io",
        StoreErrorKind::InvalidArgument => "invalid-argument",
        StoreErrorKind::StoreBusy => "store-busy",
        StoreErrorKind::Unsupported => "unsupported",
        StoreErrorKind::Corrupt => "corrupt",
        StoreErrorKind::BudgetExceeded => "budget-exceeded",
        StoreErrorKind::OutOfMemory => "out-of-memory",
        StoreErrorKind::DimensionMismatch => "dimension-mismatch",
        StoreErrorKind::EpochMismatch => "epoch-mismatch",
        StoreErrorKind::EpochUndeclared => "epoch-undeclared",
        StoreErrorKind::EpochUnstamped => "epoch-unstamped",
        StoreErrorKind::Internal => "internal",
        StoreErrorKind::EmptyBatch => "empty-batch",
        StoreErrorKind::Cancelled => "cancelled",
        StoreErrorKind::ReadOnly => "read-only",
        StoreErrorKind::Closing => "closing",
        StoreErrorKind::Closed => "closed",
        StoreErrorKind::Panic => "panic",
        StoreErrorKind::Synchronization => "synchronization",
    }
}

fn vector_generic_status_json(status: &vector_adapter::VectorGenericFaultStatus) -> String {
    match status {
        vector_adapter::VectorGenericFaultStatus::StoreResult(result) => format!(
            "{{\"kind\":\"store-result\",\"result\":{}}}",
            vector_store_result_json(result)
        ),
        vector_adapter::VectorGenericFaultStatus::StoreFailure { kind } => format!(
            "{{\"kind\":\"store-failure\",\"error_kind\":\"{}\"}}",
            vector_store_error_kind_key(*kind)
        ),
        vector_adapter::VectorGenericFaultStatus::IngestFailure { status } => format!(
            "{{\"kind\":\"ingest-failure\",\"status\":{}}}",
            vector_status_json(status)
        ),
    }
}

fn vector_generic_event_json(event: &vector_adapter::VectorGenericFaultEvent) -> String {
    let path_contains = event.path_contains.as_ref().map_or_else(
        || "null".to_owned(),
        |path| format!("\"{}\"", json_escape(path)),
    );
    let path = event.path.as_ref().map_or_else(
        || "null".to_owned(),
        |path| format!("\"{}\"", json_escape(path)),
    );
    format!(
        "{{\"id\":\"{}\",\"op_index\":{},\"site\":\"{}\",\"mode\":\"{}\",\"nth_match\":{},\"path_contains\":{path_contains},\"fired\":{},\"path\":{path}}}",
        json_escape(&event.id),
        event.op_index,
        vector_generic_fault_site_key(event.site),
        vector_generic_fault_mode_key(event.mode),
        event.nth_match,
        event.fired,
    )
}

fn vector_generic_leg_json(leg: &vector_adapter::VectorGenericFaultLeg) -> String {
    let receipts = leg
        .feature_receipts
        .iter()
        .map(|receipt| {
            production_receipt_json(&ProductionFeatureReceipt::ValidatedVectorFeature(
                receipt.clone(),
            ))
        })
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{{\"stage\":\"{}\",\"status\":{},\"event\":{},\"feature_receipts\":[{receipts}]}}",
        vector_generic_fault_stage_key(leg.stage),
        vector_generic_status_json(&leg.status),
        vector_generic_event_json(&leg.event),
    )
}

fn vector_adapter_fault_key(fault: vector_adapter::VectorFaultKind) -> &'static str {
    match fault {
        vector_adapter::VectorFaultKind::ForcedDispatchBackend => "forced-dispatch-backend",
        vector_adapter::VectorFaultKind::CorruptCodesFactors => "corrupt-codes-factors",
        vector_adapter::VectorFaultKind::MissingRescoreRows => "missing-rescore-rows",
        vector_adapter::VectorFaultKind::RowCountCancellation => "row-count-cancellation",
        vector_adapter::VectorFaultKind::AllocationDenial => "allocation-denial",
    }
}

fn vector_case_identity(case_id: u64, fault: Option<vector_adapter::VectorFaultKind>) -> String {
    fault.map_or_else(
        || format!("case-{case_id}"),
        |fault| format!("case-{case_id}-fault-{}", vector_adapter_fault_key(fault)),
    )
}

fn vector_generic_fault_json(
    generic: Option<&vector_adapter::VectorGenericFaultEvidence>,
) -> String {
    generic.map_or_else(
        || "null".to_owned(),
        |generic| {
            let feature_fault = generic.feature_fault.map_or_else(
                || "null".to_owned(),
                |fault| format!("\"{}\"", vector_adapter_fault_key(fault)),
            );
            let path_contains = generic.schedule.path_contains.as_ref().map_or_else(
                || "null".to_owned(),
                |path| format!("\"{}\"", json_escape(path)),
            );
            format!(
                "{{\"operation\":\"{}\",\"feature_fault\":{feature_fault},\"feature_mutation\":{},\"program_op_index\":{},\"schedule\":{{\"id\":\"{}\",\"site\":\"{}\",\"mode\":\"{}\",\"nth_match\":{},\"path_contains\":{path_contains}}},\"clean\":{},\"fault\":{},\"clean_initial_directory\":{},\"fault_initial_directory\":{},\"isolated_directories\":{},\"isolated_runtimes\":{}}}",
                generic.operation.key(),
                vector_mutation_json(&generic.feature_mutation),
                generic.program_op_index,
                json_escape(&generic.schedule.id),
                vector_generic_fault_site_key(generic.schedule.site),
                vector_generic_fault_mode_key(generic.schedule.mode),
                generic.schedule.nth_match,
                vector_generic_leg_json(&generic.clean),
                vector_generic_leg_json(&generic.fault),
                vector_fixture_directory_json(&generic.clean_initial_directory),
                vector_fixture_directory_json(&generic.fault_initial_directory),
                generic.isolated_directories,
                generic.isolated_runtimes,
            )
        },
    )
}

fn vector_fixture_inputs_json(inputs: &vector_adapter::VectorPrimitiveInputs) -> String {
    match inputs {
        vector_adapter::VectorPrimitiveInputs::I24(inputs) => {
            vector_json_values(inputs, vector_kernel_input_json)
        }
        vector_adapter::VectorPrimitiveInputs::I25(inputs) => {
            vector_json_values(inputs, vector_quant_input_json)
        }
        vector_adapter::VectorPrimitiveInputs::I26(inputs) => {
            vector_json_values(inputs, vector_rescore_input_json)
        }
        vector_adapter::VectorPrimitiveInputs::I27(inputs) => {
            vector_json_values(inputs, vector_identity_input_json)
        }
    }
}

fn vector_forced_child_json(child: Option<&vector_adapter::ForcedBackendChildEvidence>) -> String {
    child.map_or_else(
        || "null".to_owned(),
        |child| {
            let transport = match child.transport {
                vector_adapter::ForcedBackendTransportFormat::TypedBinaryV1 => {
                    "typed-binary-v1"
                }
            };
            let receipt = production_receipt_json(
                &ProductionFeatureReceipt::ValidatedVectorFeature(child.receipt.clone()),
            );
            format!(
                "{{\"transport\":\"{transport}\",\"requested\":\"{}\",\"pair\":{{\"input\":{},\"observed\":{}}},\"fault_result\":{},\"retry_result\":{},\"receipt\":{receipt}}}",
                vector_backend_key(child.requested),
                vector_kernel_input_json(&child.pair.input),
                vector_i24_observed_json(&child.pair.observed),
                vector_store_result_json(&child.fault_result),
                vector_store_result_json(&child.retry_result),
            )
        },
    )
}

fn record_vector_family_artifacts(
    operation: super::campaign::VectorOperation,
    evidence: &vector_adapter::VectorOperationEvidence,
    seed: u64,
    family_artifact_records: &mut BTreeMap<&'static str, Vec<String>>,
) -> Result<(), String> {
    let input_count = match &evidence.fixture.inputs {
        vector_adapter::VectorPrimitiveInputs::I24(inputs) => inputs.len(),
        vector_adapter::VectorPrimitiveInputs::I25(inputs) => inputs.len(),
        vector_adapter::VectorPrimitiveInputs::I26(inputs) => inputs.len(),
        vector_adapter::VectorPrimitiveInputs::I27(inputs) => inputs.len(),
    };
    let backends = vector_json_string_array(
        evidence
            .fixture
            .backend_inventory
            .iter()
            .map(|backend| vector_backend_key(*backend).to_owned()),
    );
    let tiers = evidence
        .fixture
        .tier_inventory
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let sources = vector_json_values(&evidence.fixture.source_inventory, |source| {
        vector_source_json(*source)
    });
    let documents = vector_json_values(&evidence.fixture.documents, |document| {
        vector_document_json(*document)
    });
    let public_schedule = vector_schedule_json(&evidence.fixture.public_schedule);
    let inputs = vector_fixture_inputs_json(&evidence.fixture.inputs);
    let forced_child = vector_forced_child_json(evidence.forced_child.as_ref());
    let generic_fault = vector_generic_fault_json(evidence.generic_fault.as_ref());
    let retained_fixture = vector_adapter::encode_vector_fixture(evidence)?;
    let retained_fixture_hex = evidence_hex(&retained_fixture);
    let retained_fixture_digest = retained_fixture
        .get(retained_fixture.len().saturating_sub(32)..)
        .ok_or_else(|| "vector retained fixture omitted its SHA-256".to_owned())?;
    family_artifact_records
        .entry("fixture.json")
        .or_default()
        .push(format!(
            "{{\"campaign\":\"vector-execution\",\"namespace\":\"{}\",\"operation\":\"{}\",\"seed\":{seed},\"input_count\":{input_count},\"inputs\":[{inputs}],\"backend_inventory\":[{backends}],\"tier_inventory\":[{tiers}],\"source_inventory\":[{sources}],\"documents\":[{documents}],\"public_schedule\":[{public_schedule}],\"forced_child\":{forced_child},\"generic_fault\":{generic_fault},\"retained_fixture_version\":\"{}\",\"retained_fixture_bytes\":{},\"retained_fixture_sha256\":\"{}\",\"retained_fixture_hex\":\"{retained_fixture_hex}\"}}",
            json_escape(evidence.fixture.namespace),
            json_escape(operation.key()),
            vector_adapter::VECTOR_FIXTURE_CODEC_VERSION,
            retained_fixture.len(),
            evidence_hex(retained_fixture_digest),
        ));

    match &evidence.invariant {
        vector_adapter::VectorInvariantEvidence::I24(pairs) => {
            let all_backends = [
                vector_oracle::BackendId::Scalar,
                vector_oracle::BackendId::NeonWiden,
                vector_oracle::BackendId::NeonDotprodU4,
                vector_oracle::BackendId::NeonI8mm,
                vector_oracle::BackendId::NeonDotprodU2,
                vector_oracle::BackendId::NeonDotprodU6,
                vector_oracle::BackendId::NeonDotprodU8,
                vector_oracle::BackendId::NeonDotprodU4Prefetch,
                vector_oracle::BackendId::Avx2,
            ];
            let available = evidence
                .fixture
                .backend_inventory
                .iter()
                .copied()
                .collect::<BTreeSet<_>>();
            let unavailable = vector_json_string_array(
                all_backends
                    .into_iter()
                    .filter(|backend| !available.contains(backend))
                    .map(|backend| vector_backend_key(backend).to_owned()),
            );
            let selected = vector_json_string_array(
                pairs
                    .iter()
                    .filter(|pair| pair.observed.selected_for_store)
                    .map(|pair| vector_backend_key(pair.observed.backend).to_owned())
                    .collect::<BTreeSet<_>>(),
            );
            let mut invocations = BTreeMap::<vector_oracle::BackendId, u64>::new();
            for pair in pairs {
                let count = invocations.entry(pair.observed.backend).or_default();
                *count = count.saturating_add(1);
            }
            let invocation_counts = invocations
                .into_iter()
                .map(|(backend, count)| format!("\"{}\":{count}", vector_backend_key(backend)))
                .collect::<Vec<_>>()
                .join(",");
            family_artifact_records
                .entry("backend-inventory.json")
                .or_default()
                .push(format!(
                    "{{\"campaign\":\"vector-execution\",\"operation\":\"kernel-parity\",\"seed\":{seed},\"host\":{{\"os\":\"{}\",\"arch\":\"{}\"}},\"available\":[{backends}],\"unavailable\":[{unavailable}],\"selected\":[{selected}],\"invocation_counts\":{{{invocation_counts}}}}}",
                    std::env::consts::OS,
                    std::env::consts::ARCH,
                ));
        }
        vector_adapter::VectorInvariantEvidence::I25(pairs) => {
            let records = family_artifact_records
                .entry("quantization.jsonl")
                .or_default();
            for pair in pairs {
                records.push(format!(
                    "{{\"campaign\":\"vector-execution\",\"operation\":\"quantization\",\"checker_id\":\"{}\",\"seed\":{seed},\"case_id\":{},\"scheme\":\"{}\",\"input\":{},\"observed\":{}}}",
                    vector_oracle::I25_CHECKER_ID,
                    pair.input.case_id,
                    vector_quant_scheme_key(pair.input.scheme),
                    vector_quant_input_json(&pair.input),
                    vector_i25_observed_json(&pair.observed),
                ));
            }
        }
        vector_adapter::VectorInvariantEvidence::I26(pairs) => {
            let records = family_artifact_records.entry("rescore.jsonl").or_default();
            for pair in pairs {
                records.push(format!(
                    "{{\"campaign\":\"vector-execution\",\"operation\":\"rescore\",\"checker_id\":\"{}\",\"seed\":{seed},\"case_id\":{},\"metric\":\"{}\",\"tier\":{},\"input\":{},\"observed\":{}}}",
                    vector_oracle::I26_CHECKER_ID,
                    pair.input.case_id,
                    vector_rescore_metric_key(pair.input.metric),
                    pair.observed.tier,
                    vector_rescore_input_json(&pair.input),
                    vector_i26_observed_json(&pair.observed),
                ));
            }
        }
        vector_adapter::VectorInvariantEvidence::I27(pairs) => {
            let records = family_artifact_records.entry("identity.jsonl").or_default();
            for pair in pairs {
                records.push(format!(
                    "{{\"campaign\":\"vector-execution\",\"operation\":\"row-identity\",\"checker_id\":\"{}\",\"seed\":{seed},\"case_id\":{},\"phase\":{},\"tier\":{},\"input\":{},\"observed\":{}}}",
                    vector_oracle::I27_CHECKER_ID,
                    pair.input.case_id,
                    pair.input.observation_phase,
                    pair.input.tier,
                    vector_identity_input_json(&pair.input),
                    vector_i27_observed_json(&pair.observed),
                ));
            }
        }
    }
    Ok(())
}

fn vector_tier_key(tier: u8) -> Result<&'static str, String> {
    match tier {
        0 => Ok("auto"),
        1 => Ok("exact"),
        2 => Ok("scan"),
        3 => Ok("graph"),
        other => Err(format!("unknown vector tier id {other}")),
    }
}

fn record_i24_coverage(pair: &vector_adapter::I24EvidencePair, coverage: &mut CoverageRegistry) {
    let backend = vector_backend_key(pair.input.backend);
    if pair.input.selected_for_store {
        coverage.hit(format!("I24.store-selected.{backend}"));
        return;
    }
    coverage.hit(format!(
        "I24.kernel.{}.backend.{backend}.dimension.{}.offset.{}",
        vector_kernel_key(pair.input.kernel),
        pair.input.dimension,
        pair.input.input_offset(),
    ));
    coverage.hit(format!("kernel.backend.{backend}"));
    match pair.input.kernel {
        vector_oracle::KernelId::DotF32 => {
            let cancellation = [1.0e20_f32, 1.0, -1.0e20, -1.0].map(f32::to_bits);
            let cancellation_present = pair.input.f32_a.len() >= cancellation.len()
                && pair.input.f32_b.len() >= cancellation.len()
                && pair.input.f32_a[..cancellation.len()]
                    .iter()
                    .map(|value| value.0)
                    .eq(cancellation)
                && pair.input.f32_b[..cancellation.len()]
                    .iter()
                    .all(|value| value.0 == 1.0_f32.to_bits());
            if cancellation_present {
                coverage.hit("I24.f32.cancellation-heavy-alternating-magnitude");
            }
            let seeded_layout_present = !pair.input.f32_a.is_empty()
                && pair.input.f32_a.len() == pair.input.f32_b.len()
                && pair
                    .input
                    .f32_a
                    .iter()
                    .chain(&pair.input.f32_b)
                    .all(|value| (119..=135).contains(&((value.0 >> 23) & 0xff)));
            if seeded_layout_present {
                coverage.hit("I24.f32.seeded-raw-finite");
            }
            for (label, present) in [
                (
                    "neg-zero",
                    pair.input.f32_a.iter().any(|value| value.0 == 0x8000_0000),
                ),
                (
                    "subnormal",
                    pair.input
                        .f32_a
                        .iter()
                        .any(|value| value.0 & 0x7f80_0000 == 0 && value.0 & 0x007f_ffff != 0),
                ),
                (
                    "pos-inf",
                    pair.input.f32_a.iter().any(|value| value.0 == 0x7f80_0000),
                ),
                (
                    "neg-inf",
                    pair.input.f32_a.iter().any(|value| value.0 == 0xff80_0000),
                ),
                (
                    "nan",
                    pair.input
                        .f32_a
                        .iter()
                        .any(|value| f32::from_bits(value.0).is_nan()),
                ),
            ] {
                if present {
                    coverage.hit(format!("I24.special.f32.{label}"));
                }
            }
        }
        vector_oracle::KernelId::DotF16 => {
            for (label, present) in [
                ("neg-zero", pair.input.f16_a.contains(&0x8000)),
                (
                    "subnormal",
                    pair.input
                        .f16_a
                        .iter()
                        .any(|value| value & 0x7c00 == 0 && value & 0x03ff != 0),
                ),
                ("pos-inf", pair.input.f16_a.contains(&0x7c00)),
                ("neg-inf", pair.input.f16_a.contains(&0xfc00)),
                (
                    "nan",
                    pair.input
                        .f16_a
                        .iter()
                        .any(|value| value & 0x7c00 == 0x7c00 && value & 0x03ff != 0),
                ),
            ] {
                if present {
                    coverage.hit(format!("I24.special.f16.{label}"));
                }
            }
        }
        _ => {}
    }
}

fn vector_nonfinite_label(value: vector_oracle::F32) -> Option<&'static str> {
    let value = value.to_float();
    if value.is_nan() {
        Some("nan")
    } else if value == f32::INFINITY {
        Some("pos-inf")
    } else if value == f32::NEG_INFINITY {
        Some("neg-inf")
    } else {
        None
    }
}

fn vector_position_label(index: usize, len: usize) -> &'static str {
    if index == 0 {
        "first"
    } else if index + 1 == len {
        "last"
    } else {
        "middle"
    }
}

fn record_i25_coverage(pair: &vector_adapter::I25EvidencePair, coverage: &mut CoverageRegistry) {
    let scheme = vector_quant_scheme_key(pair.input.scheme);
    let expected_len = match pair.input.scheme {
        vector_oracle::QuantScheme::Bit4 => pair.input.row.len().div_ceil(2),
        vector_oracle::QuantScheme::Int8 => pair.input.row.len(),
    } as u64;
    if pair.input.row.is_empty() {
        coverage.hit(format!("I25.{scheme}.empty"));
    }
    if pair.input.row.len() == 65_537 {
        coverage.hit(format!("I25.{scheme}.dimension-65537"));
    }
    if pair.input.output_len < expected_len {
        coverage.hit(format!("I25.{scheme}.output-short"));
    } else if pair.input.output_len > expected_len {
        coverage.hit(format!("I25.{scheme}.output-long"));
    }
    if pair.input.code_len < expected_len {
        coverage.hit(format!("I25.{scheme}.code-short"));
    } else if pair.input.code_len > expected_len {
        coverage.hit(format!("I25.{scheme}.code-long"));
    }
    for (side, values) in [("row", &pair.input.row), ("query", &pair.input.query)] {
        for (index, value) in values.iter().copied().enumerate() {
            if let Some(class) = vector_nonfinite_label(value) {
                coverage.hit(format!(
                    "I25.{scheme}.{side}.{class}.{}",
                    vector_position_label(index, values.len())
                ));
            }
        }
    }
    if matches!(
        pair.observed.result.status,
        vector_oracle::PrimitiveStatus::Ok
    ) && pair.observed.result.success.is_some()
    {
        let dimension_class = if pair.input.row.len().is_multiple_of(2) {
            "even"
        } else {
            "odd"
        };
        coverage.hit(format!("I25.{scheme}.positive.{dimension_class}"));
        let row_bits = pair
            .input
            .row
            .iter()
            .map(|value| value.0)
            .collect::<Vec<_>>();
        if !row_bits.is_empty() && row_bits.iter().all(|value| *value == row_bits[0]) {
            coverage.hit(format!("I25.{scheme}.positive.constant"));
        }
        if row_bits.contains(&0) && row_bits.contains(&0x8000_0000) {
            coverage.hit(format!("I25.{scheme}.positive.signed-zero"));
        }
        if row_bits
            .iter()
            .any(|bits| bits & 0x7f80_0000 == 0 && bits & 0x007f_ffff != 0)
        {
            coverage.hit(format!("I25.{scheme}.positive.subnormal"));
        }
        if row_bits
            .iter()
            .any(|bits| bits & 0x7fff_ffff == f32::MAX.to_bits())
        {
            coverage.hit(format!("I25.{scheme}.positive.extreme-finite"));
        }
        if row_bits
            == [
                (-127.0_f32).to_bits(),
                0.5_f32.to_bits(),
                127.0_f32.to_bits(),
            ]
        {
            coverage.hit(format!("I25.{scheme}.positive.halfway"));
        }
        if row_bits
            == [
                1.0_f32.to_bits(),
                1.0_f32.to_bits(),
                0.5_f32.to_bits(),
                (-0.5_f32).to_bits(),
            ]
        {
            coverage.hit(format!("I25.{scheme}.positive.threshold-tie"));
        }
    }
    match pair.input.store.schedule.as_slice() {
        [vector_oracle::PublicStoreStep::IngestAccepted]
            if pair.input.scheme == vector_oracle::QuantScheme::Bit4
                && pair.observed.store.document_visible =>
        {
            coverage.hit("I25.bit4.store-accepted-visible");
        }
        [
            vector_oracle::PublicStoreStep::IngestAccepted,
            vector_oracle::PublicStoreStep::Seal,
            vector_oracle::PublicStoreStep::Reopen,
            vector_oracle::PublicStoreStep::Search,
        ] if pair.input.scheme == vector_oracle::QuantScheme::Int8
            && pair.observed.store.generation_after
                == pair.observed.store.generation_before.saturating_add(2)
            && pair.observed.store.document_visible
            && pair.observed.store.scan_status == vector_oracle::PrimitiveStatus::Ok
            && !pair.observed.store.persisted_code_bytes.is_empty()
            && !pair.observed.store.persisted_factor_bits.is_empty() =>
        {
            coverage.hit("I25.int8.store-published");
        }
        [vector_oracle::PublicStoreStep::IngestRejected]
            if pair.input.scheme == vector_oracle::QuantScheme::Bit4
                && matches!(
                    pair.observed.store.ingest_status,
                    vector_oracle::PrimitiveStatus::NonFinite { .. }
                ) =>
        {
            coverage.hit("I25.bit4.store-rejected-nonfinite");
        }
        _ => {}
    }
}

fn record_i26_coverage(
    pair: &vector_adapter::I26EvidencePair,
    coverage: &mut CoverageRegistry,
) -> Result<(), String> {
    match &pair.input.candidates {
        vector_oracle::CandidateMode::Dense { .. } => coverage.hit("I26.mode.dense"),
        vector_oracle::CandidateMode::Retained { rows, coarse } => {
            coverage.hit("I26.mode.retained");
            if rows.len() != coarse.len() {
                coverage.hit("I26.reject.candidate-count");
            } else if rows.iter().any(|row| {
                u64::from(*row)
                    >= pair.input.rows_row_major.len() as u64 / pair.input.dimension.max(1)
            }) {
                coverage.hit("I26.reject.candidate-out-of-range");
            } else if coarse.iter().any(|score| !score.to_float().is_finite()) {
                coverage.hit("I26.reject.nonfinite-coarse");
            }
        }
    }
    if let Some(store) = &pair.input.store {
        let source = match store.source {
            vector_oracle::PrimitiveSource::Active => "active",
            vector_oracle::PrimitiveSource::Sealed(_) => "sealed",
        };
        let tier = match (source, store.tier, store.exact_rescore) {
            ("active", 0, false) => "auto-estimated",
            ("active", 1, true) => "exact",
            ("active", 2, false) => "scan",
            ("sealed", 0, true) => "auto-graph",
            ("sealed", 1, true) => "exact",
            ("sealed", 3, true) => "graph",
            _ => {
                return Err(format!(
                    "unclassified passing I26 Store cell source={source} tier={} exact_rescore={}",
                    store.tier, store.exact_rescore
                ));
            }
        };
        coverage.hit(format!("I26.store.{source}.{tier}"));
        if source == "active" && tier == "exact" {
            let physical_documents = store
                .documents_by_row
                .iter()
                .flatten()
                .copied()
                .collect::<Vec<_>>();
            let observed_documents = pair
                .observed
                .store_hits
                .iter()
                .filter_map(|hit| hit.document)
                .collect::<Vec<_>>();
            let observed_rows = pair
                .observed
                .store_hits
                .iter()
                .map(|hit| hit.row.local_row)
                .collect::<Vec<_>>();
            let equal_scores = pair.observed.store_hits.first().is_some_and(|first| {
                pair.observed
                    .store_hits
                    .iter()
                    .all(|hit| hit.score == first.score)
            });
            let physical_descending = physical_documents.windows(2).all(|pair| pair[0] > pair[1]);
            let observed_ascending = observed_documents.windows(2).all(|pair| pair[0] < pair[1]);
            let rows_descending = observed_rows.windows(2).all(|pair| pair[0] > pair[1]);
            if physical_documents.len() >= 3
                && physical_documents.len() == observed_documents.len()
                && physical_descending
                && observed_ascending
                && rows_descending
                && equal_scores
            {
                coverage.hit("I26.store.active.exact.anti-correlated-document-tie");
            }
        }
    }
    Ok(())
}

fn record_i27_coverage(
    pair: &vector_adapter::I27EvidencePair,
    coverage: &mut CoverageRegistry,
) -> Result<(), String> {
    coverage.hit(format!(
        "I27.phase.{}.tier.{}",
        pair.input.observation_phase,
        vector_tier_key(pair.input.tier)?
    ));
    let sealed = pair
        .input
        .mutations
        .iter()
        .filter_map(|mutation| match mutation {
            vector_oracle::IdentityMutation::Seal(segment) => Some(*segment),
            _ => None,
        })
        .collect::<Vec<_>>();
    for row in &pair.observed.rows {
        if row.row.local_row != 0 {
            continue;
        }
        match row.row.source {
            vector_oracle::PrimitiveSource::Active => {
                coverage.hit("I27.physical.active-row-zero");
            }
            vector_oracle::PrimitiveSource::Sealed(segment) if sealed.first() == Some(&segment) => {
                coverage.hit("I27.physical.first-sealed-row-zero");
            }
            vector_oracle::PrimitiveSource::Sealed(segment) if sealed.get(1) == Some(&segment) => {
                coverage.hit("I27.physical.second-sealed-row-zero");
            }
            vector_oracle::PrimitiveSource::Sealed(_) => {}
        }
    }
    for mutation in &pair.input.mutations {
        match mutation {
            vector_oracle::IdentityMutation::Replace(_) => {
                coverage.hit("I27.transition.replace");
            }
            vector_oracle::IdentityMutation::Delete { .. } => {
                coverage.hit("I27.transition.delete");
            }
            vector_oracle::IdentityMutation::Reopen => {
                coverage.hit("I27.transition.reopen");
            }
            vector_oracle::IdentityMutation::Ingest(_)
            | vector_oracle::IdentityMutation::Seal(_) => {}
        }
    }
    Ok(())
}

fn record_vector_inventory_coverage(coverage: &mut CoverageRegistry) {
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
    let available = KernelVariant::available()
        .map(|variant| variant.backend_id().as_str())
        .collect::<BTreeSet<_>>();
    for backend in ALL_BACKENDS {
        let state = if available.contains(backend) {
            "available"
        } else {
            "unavailable"
        };
        coverage.hit(format!("I24.backend.{state}.{backend}"));
    }
}

fn record_vector_fault_coverage(
    evidence: &vector_adapter::VectorOperationEvidence,
    coverage: &mut CoverageRegistry,
) -> Result<(), String> {
    let mutation = evidence
        .generic_fault
        .as_ref()
        .map_or(&evidence.mutation, |generic| &generic.feature_mutation);
    let key = match *mutation {
        vector_adapter::VectorMutationEvidence::None => return Ok(()),
        vector_adapter::VectorMutationEvidence::ForcedDispatch { .. } => {
            "fault.forced-backend.kernel-dispatch-selected-scoring-table"
        }
        vector_adapter::VectorMutationEvidence::QuantCorruption { field, .. } => match field {
            vector_adapter::VectorQuantMutationField::Bit4OddPadding => {
                "fault.quant.bit4-odd-padding.scan-bit4-code-view"
            }
            vector_adapter::VectorQuantMutationField::Bit4Correction => {
                "fault.quant.bit4-correction.scan-bit4-factor-view"
            }
            vector_adapter::VectorQuantMutationField::Int8Scale => {
                "fault.quant.int8-scale.scan-int8-factor-view"
            }
        },
        vector_adapter::VectorMutationEvidence::MissingRescoreRows { tier, site, .. } => {
            match (tier, site) {
                (1, vector_adapter::VectorRescoreMutationSite::ExactRescoreRows) => {
                    "fault.rescore.exact.exact-rescore-rows"
                }
                (3, vector_adapter::VectorRescoreMutationSite::QueryRescoreRows) => {
                    "fault.rescore.graph.query-rescore-rows"
                }
                _ => return Err("unclassified passing vector rescore fault".to_owned()),
            }
        }
        vector_adapter::VectorMutationEvidence::RowCancellation { source, tier, .. } => {
            match ((evidence.fixture.seed / 6) % 4, source, tier) {
                (0, vector_oracle::PrimitiveSource::Active, 1) => "fault.cancel.active-exact",
                (1, vector_oracle::PrimitiveSource::Sealed(_), 2) => {
                    "fault.cancel.sealed-bit4-scan"
                }
                (2, vector_oracle::PrimitiveSource::Sealed(_), 2) => {
                    "fault.cancel.sealed-int8-scan"
                }
                (3, vector_oracle::PrimitiveSource::Sealed(_), 3) => "fault.cancel.sealed-graph",
                _ => return Err("unclassified passing vector cancellation fault".to_owned()),
            }
        }
        vector_adapter::VectorMutationEvidence::AllocationDenial { .. } => {
            "fault.allocation.exact.search-global-candidates"
        }
    };
    coverage.hit(key);
    Ok(())
}

fn storage_episode_oracle_attestation(
    outcome: &RunOutcome,
    oracle_records: &[OracleRecord],
) -> Result<zeppelin_embed_bench::harness_json::Value, String> {
    let fixture = outcome
        .family_artifact_bytes
        .get("storage-fixture.json")
        .ok_or_else(|| "storage episode omitted storage-fixture.json".to_owned())?;
    let invariant_record = |invariant: u8,
                            checker_id: &'static str,
                            required_operation: &'static str,
                            allowed_operations: &[&'static str]| {
        let records = oracle_records
            .iter()
            .filter(|record| record.invariant == invariant)
            .collect::<Vec<_>>();
        if records.is_empty() {
            return Err(format!("storage I{invariant} episode comparison is absent"));
        }
        if records.iter().any(|record| {
            record.checker_id != checker_id
                || !allowed_operations.contains(&record.operation)
                || record.canonical_version
                    != zeppelin_embed_adversarial_oracle::ORACLE_CONTRACT_VERSION
        }) {
            return Err(format!(
                "storage I{invariant} episode records do not use the exact checker/operation/canonical contract"
            ));
        }
        let mut operations = BTreeMap::<&'static str, u64>::new();
        for record in &records {
            *operations.entry(record.operation).or_default() += 1;
        }
        if operations.get(required_operation).copied().unwrap_or(0) == 0 {
            return Err(format!(
                "storage I{invariant} episode omitted required operation {required_operation}"
            ));
        }
        let comparisons = u64::try_from(records.len())
            .map_err(|_| format!("storage I{invariant} comparison count exceeds u64"))?;
        let passes = u64::try_from(records.iter().filter(|record| record.passed).count())
            .map_err(|_| format!("storage I{invariant} pass count exceeds u64"))?;
        if comparisons != passes
            || records
                .iter()
                .any(|record| record.first_difference.is_some())
        {
            return Err(format!(
                "storage I{invariant} episode contains a failed comparison"
            ));
        }
        Ok(zeppelin_embed_bench::harness_json::json!({
            "checker_id": checker_id,
            "operations": operations,
            "canonical_version": zeppelin_embed_adversarial_oracle::ORACLE_CONTRACT_VERSION,
            "comparisons": comparisons,
            "passes": passes,
            "input_digest": super::artifacts::evidence_digest(
                &records.iter().map(|record| record.input_digest.as_bytes()).collect::<Vec<_>>()
            ),
            "observed_digest": super::artifacts::evidence_digest(
                &records.iter().map(|record| record.observed_digest.as_bytes()).collect::<Vec<_>>()
            ),
            "first_differences": comparisons.saturating_sub(passes),
        }))
    };
    let per_invariant_comparisons = BTreeMap::from([
        (
            "I15".to_owned(),
            invariant_record(
                15,
                storage_oracle::I15_CHECKER_ID,
                "publication",
                &["publication"],
            )?,
        ),
        (
            "I16".to_owned(),
            invariant_record(
                16,
                storage_oracle::I16_CHECKER_ID,
                "wal-prefix",
                &["wal-prefix"],
            )?,
        ),
        (
            "I17".to_owned(),
            invariant_record(17, storage_oracle::I17_CHECKER_ID, "retry", &["retry"])?,
        ),
        (
            "I18".to_owned(),
            invariant_record(
                18,
                storage_oracle::I18_CHECKER_ID,
                "format-check",
                &["format-check", "wal-prefix"],
            )?,
        ),
        (
            "I19".to_owned(),
            invariant_record(
                19,
                storage_oracle::I19_CHECKER_ID,
                "orphan-cleanup",
                &["orphan-cleanup"],
            )?,
        ),
    ]);

    let mut control_operations = BTreeMap::<String, u64>::new();
    for line in outcome
        .controls_bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let record: zeppelin_embed_bench::harness_json::Value =
            zeppelin_embed_bench::harness_json::from_slice(line)
                .map_err(|error| format!("parse storage control evidence: {error}"))?;
        if record["campaign"].as_str() != Some("storage-durability") {
            return Err("storage control stream contained another campaign".to_owned());
        }
        let operation = record["operation"]
            .as_str()
            .ok_or_else(|| "storage control omitted operation".to_owned())?;
        *control_operations.entry(operation.to_owned()).or_default() += 1;
    }

    let mut receipt_faults = BTreeMap::<String, u64>::new();
    let mut receipt_sites = BTreeMap::<String, u64>::new();
    let mut receipt_records = 0_u64;
    for line in outcome
        .receipts_bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let record: zeppelin_embed_bench::harness_json::Value =
            zeppelin_embed_bench::harness_json::from_slice(line)
                .map_err(|error| format!("parse storage receipt evidence: {error}"))?;
        if record["campaign"].as_str() != Some("storage-durability") {
            return Err("storage receipt stream contained another campaign".to_owned());
        }
        let fault = record["fault"]
            .as_str()
            .ok_or_else(|| "storage receipt omitted fault".to_owned())?;
        let site = record["site"]
            .as_str()
            .ok_or_else(|| "storage receipt omitted site".to_owned())?;
        let cardinality = record["cardinality"]
            .as_u64()
            .ok_or_else(|| "storage receipt omitted cardinality".to_owned())?;
        *receipt_faults.entry(fault.to_owned()).or_default() += cardinality;
        *receipt_sites.entry(site.to_owned()).or_default() += cardinality;
        receipt_records = receipt_records.saturating_add(cardinality);
    }

    let operation_counts = [
        "wal-prefix",
        "publication",
        "retry",
        "format-check",
        "orphan-cleanup",
    ]
    .into_iter()
    .map(|operation| {
        (
            operation.to_owned(),
            outcome
                .coverage
                .count(&format!("campaign.op.storage-durability.{operation}")),
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
            outcome
                .coverage
                .count(&format!("storage.format-case.{case}")),
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
            outcome.coverage.count(&format!("storage.omission.{case}")),
        )
    })
    .collect::<BTreeMap<_, _>>();
    let artifact_index_records = outcome
        .family_artifact_bytes
        .get("artifact-index.jsonl")
        .ok_or_else(|| "storage episode omitted artifact-index.jsonl".to_owned())?
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .count();

    Ok(zeppelin_embed_bench::harness_json::json!({
        "version": 1,
        "oracle_contract_version": storage_oracle::ORACLE_CONTRACT_VERSION,
        "oracle_source_digest": super::artifacts::evidence_digest(&[
            include_bytes!("../adversarial-oracle/src/storage_durability.rs")
        ]),
        "fixture_digest": super::artifacts::evidence_digest(&[fixture]),
        "per_invariant_comparisons": per_invariant_comparisons,
        "operations": operation_counts,
        "format_cases": format_cases,
        "omission_cases": omission_cases,
        "same_seed_controls": {
            "operations": control_operations,
            "qualifying_pairs": outcome.same_seed_clean_controls,
        },
        "integrated_receipts": {
            "expected": outcome.expected_feature_fault_receipts,
            "observed": outcome.integrated_feature_fault_receipts,
            "records": receipt_records,
            "faults": receipt_faults,
            "sites": receipt_sites,
        },
        "retained_artifacts": {
            "index_records": artifact_index_records,
        },
        "host": {"os": std::env::consts::OS, "arch": std::env::consts::ARCH},
    }))
}

fn ingest_episode_oracle_attestation(
    outcome: &RunOutcome,
    oracle_records: &[OracleRecord],
) -> Result<zeppelin_embed_bench::harness_json::Value, String> {
    let invariant_record = |invariant: u8,
                            checker_id: &'static str,
                            required_operation: &'static str| {
        let records = oracle_records
            .iter()
            .filter(|record| record.invariant == invariant)
            .collect::<Vec<_>>();
        if records.is_empty() {
            return Err(format!(
                "ingest-retention I{invariant} episode comparison is absent"
            ));
        }
        if records.iter().any(|record| {
            record.checker_id != checker_id
                || record.operation != required_operation
                || !record.passed
                || record.first_difference.is_some()
                || record.canonical_version != ingest_oracle::INGEST_CANONICAL_VERSION
                || !record.oracle_input_digest.starts_with("ingest-v1:")
                || !record.oracle_observed_digest.starts_with("ingest-v1:")
                || record.oracle_input_bytes.is_empty()
                || record.oracle_observed_bytes.is_empty()
        }) {
            return Err(format!(
                "ingest-retention I{invariant} episode record differs from its exact checker/operation contract"
            ));
        }
        let comparisons = u64::try_from(records.len())
            .map_err(|_| format!("ingest-retention I{invariant} comparison count exceeds u64"))?;
        Ok(zeppelin_embed_bench::harness_json::json!({
            "checker_id": checker_id,
            "operation": required_operation,
            "canonical_version": ingest_oracle::INGEST_CANONICAL_VERSION,
            "comparisons": comparisons,
            "passes": comparisons,
            "input_digest": super::artifacts::evidence_digest(
                &records.iter().map(|record| record.oracle_input_digest.as_bytes()).collect::<Vec<_>>()
            ),
            "observed_digest": super::artifacts::evidence_digest(
                &records.iter().map(|record| record.oracle_observed_digest.as_bytes()).collect::<Vec<_>>()
            ),
            "first_differences": 0,
        }))
    };
    let per_invariant_comparisons = BTreeMap::from([
        (
            "I20".to_owned(),
            invariant_record(20, ingest_oracle::I20_CHECKER_ID, "batch-commit")?,
        ),
        (
            "I21".to_owned(),
            invariant_record(21, ingest_oracle::I21_CHECKER_ID, "seal")?,
        ),
        (
            "I22".to_owned(),
            invariant_record(22, ingest_oracle::I22_CHECKER_ID, "retention")?,
        ),
        (
            "I23".to_owned(),
            invariant_record(23, ingest_oracle::I23_CHECKER_ID, "purge")?,
        ),
    ]);
    let operations = ["batch-commit", "seal", "retention", "purge"]
        .into_iter()
        .map(|operation| {
            (
                operation.to_owned(),
                outcome
                    .coverage
                    .count(&format!("campaign.op.ingest-retention.{operation}")),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut control_faults = BTreeMap::<String, u64>::new();
    for line in outcome
        .controls_bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let record: zeppelin_embed_bench::harness_json::Value =
            zeppelin_embed_bench::harness_json::from_slice(line)
                .map_err(|error| format!("parse ingest-retention control evidence: {error}"))?;
        if record["campaign"].as_str() != Some("ingest-retention") {
            return Err("ingest-retention control stream contained another campaign".to_owned());
        }
        let fault = record["fault"]
            .as_str()
            .ok_or_else(|| "ingest-retention control omitted fault".to_owned())?;
        *control_faults.entry(fault.to_owned()).or_default() += 1;
    }
    let mut receipt_faults = BTreeMap::<String, u64>::new();
    let mut receipt_sites = BTreeMap::<String, u64>::new();
    let mut receipt_records = 0_u64;
    for line in outcome
        .receipts_bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let record: zeppelin_embed_bench::harness_json::Value =
            zeppelin_embed_bench::harness_json::from_slice(line)
                .map_err(|error| format!("parse ingest-retention receipt evidence: {error}"))?;
        if record["campaign"].as_str() != Some("ingest-retention") {
            return Err("ingest-retention receipt stream contained another campaign".to_owned());
        }
        let fault = record["fault"]
            .as_str()
            .ok_or_else(|| "ingest-retention receipt omitted fault".to_owned())?;
        let site = record["site"]
            .as_str()
            .ok_or_else(|| "ingest-retention receipt omitted site".to_owned())?;
        let cardinality = record["cardinality"]
            .as_u64()
            .ok_or_else(|| "ingest-retention receipt omitted cardinality".to_owned())?;
        *receipt_faults.entry(fault.to_owned()).or_default() += cardinality;
        *receipt_sites.entry(site.to_owned()).or_default() += cardinality;
        receipt_records = receipt_records.saturating_add(cardinality);
    }
    Ok(zeppelin_embed_bench::harness_json::json!({
        "version": 1,
        "oracle_contract_version": ingest_oracle::ORACLE_CONTRACT_VERSION,
        "oracle_source_digest": super::artifacts::evidence_digest(&[
            include_bytes!("../adversarial-oracle/src/ingest_retention.rs")
        ]),
        "per_invariant_comparisons": per_invariant_comparisons,
        "operations": operations,
        "same_seed_controls": {
            "faults": control_faults,
            "qualifying_pairs": outcome.same_seed_clean_controls,
        },
        "integrated_receipts": {
            "expected": outcome.expected_feature_fault_receipts,
            "observed": outcome.integrated_feature_fault_receipts,
            "records": receipt_records,
            "faults": receipt_faults,
            "sites": receipt_sites,
        },
        "host": {"os": std::env::consts::OS, "arch": std::env::consts::ARCH},
    }))
}

fn vector_episode_oracle_attestation(
    outcome: &RunOutcome,
    oracle_records: &[OracleRecord],
) -> Result<zeppelin_embed_bench::harness_json::Value, String> {
    let fixture = outcome
        .family_artifact_bytes
        .get("fixture.json")
        .ok_or_else(|| "vector episode omitted fixture.json".to_owned())?;
    let invariant_record = |invariant: u8, checker_id: &'static str| {
        let records = oracle_records
            .iter()
            .filter(|record| record.invariant == invariant)
            .collect::<Vec<_>>();
        if records.iter().any(|record| {
            record.checker_id != checker_id
                || record.canonical_version != 1
                || !record
                    .oracle_input_digest
                    .starts_with(vector_oracle::VECTOR_CANONICAL_VERSION)
                || !record
                    .oracle_observed_digest
                    .starts_with(vector_oracle::VECTOR_CANONICAL_VERSION)
        }) {
            return Err(format!(
                "vector I{invariant} episode records do not use the family canonical contract"
            ));
        }
        let comparisons = u64::try_from(records.len())
            .map_err(|_| format!("vector I{invariant} comparison count exceeds u64"))?;
        let passes = u64::try_from(records.iter().filter(|record| record.passed).count())
            .map_err(|_| format!("vector I{invariant} pass count exceeds u64"))?;
        let input_digest = super::artifacts::evidence_digest(
            &records
                .iter()
                .map(|record| record.oracle_input_digest.as_bytes())
                .collect::<Vec<_>>(),
        );
        let observed_digest = super::artifacts::evidence_digest(
            &records
                .iter()
                .map(|record| record.oracle_observed_digest.as_bytes())
                .collect::<Vec<_>>(),
        );
        Ok(zeppelin_embed_bench::harness_json::json!({
            "checker_id": checker_id,
            "canonical_version": vector_oracle::VECTOR_CANONICAL_VERSION,
            "comparisons": comparisons,
            "passes": passes,
            "input_digest": input_digest,
            "observed_digest": observed_digest,
            "first_differences": comparisons.saturating_sub(passes),
        }))
    };
    let per_invariant_comparisons = BTreeMap::from([
        (
            "I24".to_owned(),
            invariant_record(24, vector_oracle::I24_CHECKER_ID)?,
        ),
        (
            "I25".to_owned(),
            invariant_record(25, vector_oracle::I25_CHECKER_ID)?,
        ),
        (
            "I26".to_owned(),
            invariant_record(26, vector_oracle::I26_CHECKER_ID)?,
        ),
        (
            "I27".to_owned(),
            invariant_record(27, vector_oracle::I27_CHECKER_ID)?,
        ),
    ]);

    let mut operations = BTreeMap::<String, u64>::new();
    let mut generic_scheduled = 0_u64;
    let mut generic_clean_fired = 0_u64;
    let mut generic_fault_fired = 0_u64;
    let mut generic_same_path = 0_u64;
    let mut generic_isolated_directories = 0_u64;
    let mut generic_isolated_runtimes = 0_u64;
    let mut generic_feature_receipts = 0_u64;
    for line in outcome
        .controls_bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let record: zeppelin_embed_bench::harness_json::Value =
            zeppelin_embed_bench::harness_json::from_slice(line)
                .map_err(|error| format!("parse vector control evidence: {error}"))?;
        let operation = record["operation"]
            .as_str()
            .ok_or_else(|| "vector control omitted operation".to_owned())?;
        let count = operations.entry(operation.to_owned()).or_default();
        *count = count.saturating_add(1);
        let generic = &record["generic_fault"];
        if !generic.is_null() {
            generic_scheduled = generic_scheduled.saturating_add(1);
            let clean_event = &generic["clean"]["event"];
            let fault_event = &generic["fault"]["event"];
            if clean_event["fired"].as_bool() == Some(true) {
                generic_clean_fired = generic_clean_fired.saturating_add(1);
            }
            if fault_event["fired"].as_bool() == Some(true) {
                generic_fault_fired = generic_fault_fired.saturating_add(1);
            }
            if clean_event["path"].as_str().is_some() && clean_event["path"] == fault_event["path"]
            {
                generic_same_path = generic_same_path.saturating_add(1);
            }
            if generic["isolated_directories"].as_bool() == Some(true)
                && generic["clean_initial_directory"] == generic["fault_initial_directory"]
            {
                generic_isolated_directories = generic_isolated_directories.saturating_add(1);
            }
            if generic["isolated_runtimes"].as_bool() == Some(true) {
                generic_isolated_runtimes = generic_isolated_runtimes.saturating_add(1);
            }
            let receipts = generic["fault"]["feature_receipts"]
                .as_array()
                .ok_or_else(|| "vector generic fault receipt ledger is absent".to_owned())?;
            generic_feature_receipts = generic_feature_receipts.saturating_add(
                u64::try_from(receipts.len())
                    .map_err(|_| "vector generic receipt count exceeds u64".to_owned())?,
            );
        }
    }
    let mut receipt_faults = BTreeMap::<String, u64>::new();
    let mut receipt_sites = BTreeMap::<String, u64>::new();
    let mut receipt_records = 0_u64;
    for line in outcome
        .receipts_bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let record: zeppelin_embed_bench::harness_json::Value =
            zeppelin_embed_bench::harness_json::from_slice(line)
                .map_err(|error| format!("parse vector receipt evidence: {error}"))?;
        if record["campaign"].as_str() != Some("vector-execution") {
            return Err("vector receipt stream contained another campaign".to_owned());
        }
        let fault = record["fault"]
            .as_str()
            .ok_or_else(|| "vector receipt omitted fault".to_owned())?;
        let site = record["site"]
            .as_str()
            .ok_or_else(|| "vector receipt omitted site".to_owned())?;
        let cardinality = record["cardinality"]
            .as_u64()
            .ok_or_else(|| "vector receipt omitted cardinality".to_owned())?;
        *receipt_faults.entry(fault.to_owned()).or_default() = receipt_faults
            .get(fault)
            .copied()
            .unwrap_or(0)
            .saturating_add(cardinality);
        *receipt_sites.entry(site.to_owned()).or_default() = receipt_sites
            .get(site)
            .copied()
            .unwrap_or(0)
            .saturating_add(cardinality);
        receipt_records = receipt_records.saturating_add(cardinality);
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
    ];
    let available = zeppelin_embed::kernels::KernelVariant::available()
        .map(|variant| variant.backend_id().as_str().to_owned())
        .collect::<BTreeSet<_>>();
    let unavailable = all_backends
        .into_iter()
        .filter(|backend| !available.contains(*backend))
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let selected = available
        .iter()
        .filter(|backend| {
            outcome
                .coverage
                .count(&format!("I24.store-selected.{backend}"))
                > 0
        })
        .cloned()
        .collect::<BTreeSet<_>>();
    let observed = available
        .iter()
        .filter(|backend| outcome.coverage.count(&format!("kernel.backend.{backend}")) > 0)
        .cloned()
        .collect::<BTreeSet<_>>();
    let features = zeppelin_embed::kernels::detected_features();

    Ok(zeppelin_embed_bench::harness_json::json!({
        "version": 1,
        "oracle_contract": vector_oracle::VECTOR_ORACLE_CONTRACT,
        "canonical_contract": vector_oracle::VECTOR_CANONICAL_VERSION,
        "fixture_digest": super::artifacts::evidence_digest(&[fixture]),
        "per_invariant_comparisons": per_invariant_comparisons,
        "same_seed_controls": {
            "operations": operations,
            "pairs": outcome.same_seed_clean_controls,
            "isolated_directories": outcome.coverage.count("vector.control.isolated-directories"),
            "byte_identical": outcome.coverage.count("vector.control.byte-identical"),
        },
        "integrated_receipts": {
            "expected": outcome.expected_feature_fault_receipts,
            "observed": outcome.integrated_feature_fault_receipts,
            "records": receipt_records,
            "faults": receipt_faults,
            "sites": receipt_sites,
        },
        "generic_fault_pairs": {
            "scheduled": generic_scheduled,
            "clean_fired": generic_clean_fired,
            "fault_fired": generic_fault_fired,
            "same_path": generic_same_path,
            "isolated_directories": generic_isolated_directories,
            "isolated_runtimes": generic_isolated_runtimes,
            "typed_feature_receipts": generic_feature_receipts,
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
    }))
}

#[allow(
    clippy::too_many_arguments,
    reason = "one vector comparison owns all exact replay evidence streams"
)]
fn record_vector_evidence(
    operation: super::campaign::VectorOperation,
    evidence: vector_adapter::VectorOperationEvidence,
    seed: u64,
    profile: FaultProfile,
    op_index: usize,
    oracle_records: &mut Vec<OracleRecord>,
    control_records: &mut Vec<String>,
    mutation_records: &mut Vec<String>,
    family_artifact_records: &mut BTreeMap<&'static str, Vec<String>>,
    coverage: &mut CoverageRegistry,
) -> Result<Vec<ProductionFeatureReceipt>, String> {
    let expected_operation = vector_operation_kind(operation);
    if evidence.operation != expected_operation
        || evidence.fixture.seed != seed
        || evidence.fixture.namespace != "vector-execution-v1"
    {
        return Err(format!(
            "vector adapter identity mismatch: expected operation={expected_operation:?} seed={seed}, observed operation={:?} fixture={:?}",
            evidence.operation, evidence.fixture
        ));
    }
    let receipts = validate_vector_receipts(&evidence)?;
    let mut canonical_attestations = BTreeMap::new();
    for attestation in evidence.canonical_attestations()? {
        let key = (attestation.checker_id, attestation.case_id);
        if canonical_attestations.insert(key, attestation).is_some() {
            return Err(format!(
                "vector family emitted duplicate canonical attestation checker={} case={}",
                key.0, key.1
            ));
        }
    }
    record_vector_family_artifacts(operation, &evidence, seed, family_artifact_records)?;
    let provenance = format!(
        "vector-execution-oracle-v1 seed={seed} profile={} op={op_index} operation={}",
        profile.key(),
        operation.key()
    );
    let mut all_checks_passed = true;
    match &evidence.invariant {
        vector_adapter::VectorInvariantEvidence::I24(pairs) => {
            if operation != super::campaign::VectorOperation::KernelParity {
                return Err("vector I24 evidence escaped kernel-parity".to_owned());
            }
            for pair in pairs {
                let expected = vector_oracle::expected_kernel(&pair.input)?;
                let case_provenance = format!("{provenance} case={}", pair.input.case_id);
                let result = vector_oracle::check_i24(&expected, &pair.observed);
                let passed = result.is_ok();
                let attestation = canonical_attestations
                    .remove(&(vector_oracle::I24_CHECKER_ID, pair.input.case_id))
                    .ok_or_else(|| {
                        format!(
                            "vector I24 case {} omitted its canonical attestation",
                            pair.input.case_id
                        )
                    })?;
                push_vector_feature_json_record(
                    24,
                    vector_oracle::I24_CHECKER_ID,
                    operation.key(),
                    vector_case_identity(pair.input.case_id, evidence.fault),
                    vector_i24_expected_json(&expected),
                    vector_i24_observed_json(&pair.observed),
                    case_provenance,
                    attestation,
                    result,
                    oracle_records,
                    coverage,
                );
                if passed {
                    record_i24_coverage(pair, coverage);
                } else {
                    all_checks_passed = false;
                }
            }
            if all_checks_passed {
                record_vector_inventory_coverage(coverage);
            }
        }
        vector_adapter::VectorInvariantEvidence::I25(pairs) => {
            if operation != super::campaign::VectorOperation::Quantization {
                return Err("vector I25 evidence escaped quantization".to_owned());
            }
            for pair in pairs {
                let expected = vector_oracle::expected_quantization(&pair.input);
                let case_provenance = format!("{provenance} case={}", pair.input.case_id);
                let result = vector_oracle::check_i25(&expected, &pair.observed);
                let passed = result.is_ok();
                let attestation = canonical_attestations
                    .remove(&(vector_oracle::I25_CHECKER_ID, pair.input.case_id))
                    .ok_or_else(|| {
                        format!(
                            "vector I25 case {} omitted its canonical attestation",
                            pair.input.case_id
                        )
                    })?;
                push_vector_feature_json_record(
                    25,
                    vector_oracle::I25_CHECKER_ID,
                    operation.key(),
                    vector_case_identity(pair.input.case_id, evidence.fault),
                    vector_i25_expected_json(&expected),
                    vector_i25_observed_json(&pair.observed),
                    case_provenance,
                    attestation,
                    result,
                    oracle_records,
                    coverage,
                );
                if passed {
                    record_i25_coverage(pair, coverage);
                } else {
                    all_checks_passed = false;
                }
            }
            if all_checks_passed {
                let stochastic = pairs
                    .iter()
                    .filter(|pair| {
                        pair.input.scheme == vector_oracle::QuantScheme::Bit4
                            && pair.input.row.len() == 7
                            && matches!(
                                pair.observed.result.status,
                                vector_oracle::PrimitiveStatus::Ok
                            )
                    })
                    .filter_map(|pair| {
                        pair.observed.result.success.as_ref().map(|success| {
                            (pair.input.query_seed, success.query_code_bytes.clone())
                        })
                    })
                    .collect::<Vec<_>>();
                let seeds = stochastic
                    .iter()
                    .map(|(seed, _)| *seed)
                    .collect::<BTreeSet<_>>();
                let query_bytes = stochastic
                    .iter()
                    .map(|(_, bytes)| bytes.clone())
                    .collect::<BTreeSet<_>>();
                if stochastic.len() == 4 && seeds.len() == 4 && query_bytes.len() == 4 {
                    coverage.hit("I25.bit4.stochastic-query.distinct-four");
                }
            }
        }
        vector_adapter::VectorInvariantEvidence::I26(pairs) => {
            if operation != super::campaign::VectorOperation::Rescore {
                return Err("vector I26 evidence escaped rescore".to_owned());
            }
            for pair in pairs {
                let expected = vector_oracle::expected_rescore(&pair.input);
                let case_provenance = format!("{provenance} case={}", pair.input.case_id);
                let result = vector_oracle::check_i26(&expected, &pair.observed);
                let passed = result.is_ok();
                let attestation = canonical_attestations
                    .remove(&(vector_oracle::I26_CHECKER_ID, pair.input.case_id))
                    .ok_or_else(|| {
                        format!(
                            "vector I26 case {} omitted its canonical attestation",
                            pair.input.case_id
                        )
                    })?;
                push_vector_feature_json_record(
                    26,
                    vector_oracle::I26_CHECKER_ID,
                    operation.key(),
                    vector_case_identity(pair.input.case_id, evidence.fault),
                    vector_i26_expected_json(&expected),
                    vector_i26_observed_json(&pair.observed),
                    case_provenance,
                    attestation,
                    result,
                    oracle_records,
                    coverage,
                );
                if passed {
                    record_i26_coverage(pair, coverage)?;
                } else {
                    all_checks_passed = false;
                }
            }
        }
        vector_adapter::VectorInvariantEvidence::I27(pairs) => {
            if operation != super::campaign::VectorOperation::RowIdentity {
                return Err("vector I27 evidence escaped row-identity".to_owned());
            }
            for pair in pairs {
                let expected = vector_oracle::expected_identity(&pair.input)?;
                let case_provenance = format!("{provenance} case={}", pair.input.case_id);
                let result = vector_oracle::check_i27(&expected, &pair.observed);
                let passed = result.is_ok();
                let attestation = canonical_attestations
                    .remove(&(vector_oracle::I27_CHECKER_ID, pair.input.case_id))
                    .ok_or_else(|| {
                        format!(
                            "vector I27 case {} omitted its canonical attestation",
                            pair.input.case_id
                        )
                    })?;
                push_vector_feature_json_record(
                    27,
                    vector_oracle::I27_CHECKER_ID,
                    operation.key(),
                    vector_case_identity(pair.input.case_id, evidence.fault),
                    vector_i27_expected_json(&expected),
                    vector_i27_observed_json(&pair.observed),
                    case_provenance,
                    attestation,
                    result,
                    oracle_records,
                    coverage,
                );
                if passed {
                    record_i27_coverage(pair, coverage)?;
                } else {
                    all_checks_passed = false;
                }
            }
        }
    }
    if !canonical_attestations.is_empty() {
        return Err(format!(
            "vector family emitted unconsumed canonical attestations: {:?}",
            canonical_attestations.keys().collect::<Vec<_>>()
        ));
    }
    if all_checks_passed {
        coverage.hit("vector.control.isolated-directories");
        coverage.hit("vector.control.byte-identical");
        if let Some(generic) = &evidence.generic_fault {
            coverage.hit("vector.generic-pair.scheduled");
            if generic.clean.event.fired {
                coverage.hit("vector.generic-pair.clean-fired");
            }
            if generic.fault.event.fired {
                coverage.hit("vector.generic-pair.fault-fired");
            }
            if generic.clean.event.path.is_some()
                && generic.clean.event.path == generic.fault.event.path
            {
                coverage.hit("vector.generic-pair.same-path");
            }
            if generic.isolated_directories
                && generic.clean_initial_directory == generic.fault_initial_directory
            {
                coverage.hit("vector.generic-pair.isolated-directories");
            }
            if generic.isolated_runtimes {
                coverage.hit("vector.generic-pair.isolated-runtimes");
            }
            for _ in &generic.fault.feature_receipts {
                coverage.hit("vector.generic-pair.typed-feature-receipt");
            }
        }
        record_vector_fault_coverage(&evidence, coverage)?;
    }
    control_records.push(format!(
        "{{\"campaign\":\"vector-execution\",\"operation\":\"{}\",\"seed\":{seed},\"control\":{},\"generic_fault\":{}}}",
        json_escape(operation.key()),
        vector_control_json(&evidence.control),
        vector_generic_fault_json(evidence.generic_fault.as_ref()),
    ));
    mutation_records.push(format!(
        "{{\"campaign\":\"vector-execution\",\"operation\":\"{}\",\"seed\":{seed},\"mutation\":{},\"paired_mutation\":{}}}",
        json_escape(operation.key()),
        vector_mutation_json(&evidence.mutation),
        evidence.generic_fault.as_ref().map_or_else(
            || "null".to_owned(),
            |generic| vector_mutation_json(&generic.feature_mutation),
        ),
    ));
    Ok(receipts)
}

#[allow(
    clippy::too_many_arguments,
    reason = "the shared adapter boundary carries every replay evidence stream"
)]
fn run_vector_campaign_operation(
    operation: super::campaign::VectorOperation,
    selected_faults: &[super::campaign::FeatureFault],
    generic_fault: Option<&FaultEvent>,
    seed: u64,
    profile: FaultProfile,
    op_index: usize,
    oracle_records: &mut Vec<OracleRecord>,
    control_records: &mut Vec<String>,
    mutation_records: &mut Vec<String>,
    family_artifact_records: &mut BTreeMap<&'static str, Vec<String>>,
    coverage: &mut CoverageRegistry,
) -> Result<Vec<ProductionFeatureReceipt>, String> {
    for name in super::artifacts::VECTOR_REPLAY_ARTIFACTS {
        if matches!(
            name,
            "coverage.jsonl" | "violations.jsonl" | "episode-summary.json"
        ) {
            continue;
        }
        family_artifact_records.entry(name).or_default();
    }
    let matching_faults = selected_faults
        .iter()
        .copied()
        .filter(|fault| fault.operation() == super::campaign::FeatureOperation::Vector(operation))
        .map(vector_fault_kind)
        .map(|fault| fault.map(Some))
        .collect::<Result<Vec<_>, _>>()?;
    let cases = if matching_faults.is_empty() {
        vec![None]
    } else {
        matching_faults
    };
    let mut receipts = Vec::new();
    for fault in cases {
        let context = vector_adapter::VectorExecutionContext {
            program_op_index: op_index,
            generic_fault: fault
                .and(generic_fault)
                .filter(|event| event.op_index == op_index)
                .map(vector_generic_fault_schedule)
                .transpose()?,
        };
        let evidence = vector_adapter::run_vector_operation_with_context(
            vector_operation_kind(operation),
            seed,
            fault,
            context,
        )?;
        receipts.extend(record_vector_evidence(
            operation,
            evidence,
            seed,
            profile,
            op_index,
            oracle_records,
            control_records,
            mutation_records,
            family_artifact_records,
            coverage,
        )?);
    }
    Ok(receipts)
}

fn vector_backend_id(backend: KernelBackendId) -> vector_oracle::BackendId {
    match backend {
        KernelBackendId::Scalar => vector_oracle::BackendId::Scalar,
        KernelBackendId::NeonWiden => vector_oracle::BackendId::NeonWiden,
        KernelBackendId::NeonDotprodU4 => vector_oracle::BackendId::NeonDotprodU4,
        KernelBackendId::NeonI8mm => vector_oracle::BackendId::NeonI8mm,
        KernelBackendId::NeonDotprodU2 => vector_oracle::BackendId::NeonDotprodU2,
        KernelBackendId::NeonDotprodU6 => vector_oracle::BackendId::NeonDotprodU6,
        KernelBackendId::NeonDotprodU8 => vector_oracle::BackendId::NeonDotprodU8,
        KernelBackendId::NeonDotprodU4Prefetch => vector_oracle::BackendId::NeonDotprodU4Prefetch,
        KernelBackendId::Avx2 => vector_oracle::BackendId::Avx2,
    }
}

fn vector_generic_fault_schedule(
    event: &FaultEvent,
) -> Result<vector_adapter::VectorGenericFaultSchedule, String> {
    let site = match event.site {
        fault_vfs::FaultSite::Admission | fault_vfs::FaultSite::GraphHop => {
            return Err(format!(
                "Cancel-only site {} cannot be adapted as a vector VFS fault",
                event.site.key()
            ));
        }
        fault_vfs::FaultSite::Open => vector_adapter::VectorGenericFaultSite::Open,
        fault_vfs::FaultSite::Read => vector_adapter::VectorGenericFaultSite::Read,
        fault_vfs::FaultSite::ReadRange => vector_adapter::VectorGenericFaultSite::ReadRange,
        fault_vfs::FaultSite::Write => vector_adapter::VectorGenericFaultSite::Write,
        fault_vfs::FaultSite::Append => vector_adapter::VectorGenericFaultSite::Append,
        fault_vfs::FaultSite::Sync => vector_adapter::VectorGenericFaultSite::Sync,
        fault_vfs::FaultSite::Rename => vector_adapter::VectorGenericFaultSite::Rename,
        fault_vfs::FaultSite::List => vector_adapter::VectorGenericFaultSite::List,
        fault_vfs::FaultSite::Delete => vector_adapter::VectorGenericFaultSite::Delete,
        fault_vfs::FaultSite::Clock => {
            return Err(format!(
                "clock fault {} cannot be routed through the vector VFS adapter",
                event.id
            ));
        }
    };
    let mode = match event.mode {
        fault_vfs::FaultMode::Eio => vector_adapter::VectorGenericFaultMode::Eio,
        fault_vfs::FaultMode::Eacces => vector_adapter::VectorGenericFaultMode::Eacces,
        fault_vfs::FaultMode::Enospc => vector_adapter::VectorGenericFaultMode::Enospc,
        fault_vfs::FaultMode::BitFlip => vector_adapter::VectorGenericFaultMode::BitFlip,
        fault_vfs::FaultMode::TornWrite => vector_adapter::VectorGenericFaultMode::TornWrite,
        fault_vfs::FaultMode::Truncate => vector_adapter::VectorGenericFaultMode::Truncate,
        fault_vfs::FaultMode::WrongObject => vector_adapter::VectorGenericFaultMode::WrongObject,
        fault_vfs::FaultMode::MisdirectedWrite => {
            vector_adapter::VectorGenericFaultMode::MisdirectedWrite
        }
        fault_vfs::FaultMode::ZeroFill => vector_adapter::VectorGenericFaultMode::ZeroFill,
        fault_vfs::FaultMode::Latency => vector_adapter::VectorGenericFaultMode::Latency,
        fault_vfs::FaultMode::SilentDrop => vector_adapter::VectorGenericFaultMode::SilentDrop,
        fault_vfs::FaultMode::PostCommitError => {
            vector_adapter::VectorGenericFaultMode::PostCommitError
        }
        fault_vfs::FaultMode::SecondOpenerInProcess | fault_vfs::FaultMode::SpawnInFlight => {
            return Err("runner-only busy fault reached vector adapter".to_owned());
        }
        fault_vfs::FaultMode::Cancel => {
            return Err("Cancel events are executed by the query runner".to_owned());
        }
        fault_vfs::FaultMode::ClockJump { .. } | fault_vfs::FaultMode::ClockStall => {
            return Err(format!(
                "clock fault {} cannot be routed through the vector VFS adapter",
                event.id
            ));
        }
        fault_vfs::FaultMode::Crash => {
            return Err("vector feature adapter cannot execute a crash-layer event".to_owned());
        }
    };
    if event.nth_match == 0 {
        return Err(format!(
            "generic fault {} has an invalid zero match index",
            event.id
        ));
    }
    Ok(vector_adapter::VectorGenericFaultSchedule {
        id: event.id.clone(),
        site,
        mode,
        nth_match: event.nth_match,
        path_contains: event.path_contains.clone(),
    })
}

#[allow(
    clippy::too_many_arguments,
    reason = "an oracle record binds the complete replay identity and exact comparison"
)]
fn push_feature_record<T: std::fmt::Debug, U: std::fmt::Debug, E: std::fmt::Display>(
    invariant: u8,
    checker_id: &'static str,
    operation: &'static str,
    expected: &T,
    observed: &U,
    provenance: String,
    result: Result<(), E>,
    records: &mut Vec<OracleRecord>,
    coverage: &mut CoverageRegistry,
) {
    push_feature_json_record(
        invariant,
        checker_id,
        operation,
        format!("\"{}\"", json_escape(&format!("{expected:?}"))),
        format!("\"{}\"", json_escape(&format!("{observed:?}"))),
        provenance,
        result,
        records,
        coverage,
    );
}

fn ingest_first_difference(
    difference: ingest_oracle::IngestFirstDifference,
) -> OracleFirstDifference {
    OracleFirstDifference {
        checker_id: difference.checker_id,
        path: difference.path,
        kind: "contract-mismatch".to_owned(),
        row: None,
        expected: difference.expected,
        observed: difference.observed,
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "ingest records bind the complete exact DTOs and family canonical bytes"
)]
fn push_ingest_feature_record<T: std::fmt::Debug, U: std::fmt::Debug, E: std::fmt::Display>(
    invariant: u8,
    checker_id: &'static str,
    operation: &'static str,
    case_identity: String,
    expected: &T,
    observed: &U,
    provenance: String,
    attestation: ingest_oracle::OracleAttestation,
    result: Result<(), E>,
    records: &mut Vec<OracleRecord>,
    coverage: &mut CoverageRegistry,
) {
    let comparison_passed = result.is_ok();
    assert_eq!(
        attestation.checker_id, checker_id,
        "ingest family attestation used a different checker"
    );
    assert_eq!(
        attestation.first_difference.is_none(),
        comparison_passed,
        "ingest family first difference disagreed with comparator status"
    );
    let position = records.len();
    push_feature_record(
        invariant, checker_id, operation, expected, observed, provenance, result, records, coverage,
    );
    let record = records
        .get_mut(position)
        .expect("push_feature_record appends exactly one ingest record");
    record.canonical_version = attestation.canonical_version;
    record.oracle_input_digest = format!(
        "ingest-v{}:{:016x}",
        attestation.canonical_version, attestation.input_digest
    );
    record.oracle_observed_digest = format!(
        "ingest-v{}:{:016x}",
        attestation.canonical_version, attestation.observed_digest
    );
    record.oracle_input_bytes = evidence_hex(&attestation.input_bytes);
    record.oracle_observed_bytes = evidence_hex(&attestation.observed_bytes);
    record.case_identity = Some(case_identity);
    record.first_difference = attestation.first_difference.map(ingest_first_difference);
}

#[allow(
    clippy::too_many_arguments,
    reason = "an oracle record binds the complete replay identity and exact comparison"
)]
fn push_feature_json_record<E: std::fmt::Display>(
    invariant: u8,
    checker_id: &'static str,
    operation: &'static str,
    expected: String,
    observed: String,
    provenance: String,
    result: Result<(), E>,
    records: &mut Vec<OracleRecord>,
    coverage: &mut CoverageRegistry,
) {
    let canonicalize = |label: &str, value: String| {
        let parsed: zeppelin_embed_bench::harness_json::Value =
            zeppelin_embed_bench::harness_json::from_str(&value)
                .unwrap_or_else(|error| panic!("{label} is not valid JSON: {error}"));
        String::from_utf8(
            zeppelin_embed_bench::harness_json::to_vec(&parsed)
                .unwrap_or_else(|error| panic!("canonicalize {label}: {error}")),
        )
        .unwrap_or_else(|error| panic!("canonical {label} is not UTF-8: {error}"))
    };
    let expected = canonicalize("oracle expected value", expected);
    let observed = canonicalize("oracle observed value", observed);
    let input_digest = super::artifacts::evidence_digest(&[expected.as_bytes()]);
    let observed_digest = super::artifacts::evidence_digest(&[observed.as_bytes()]);
    let difference_detail = result.as_ref().err().map(ToString::to_string);
    let detail = difference_detail
        .clone()
        .unwrap_or_else(|| "exact comparison succeeded".to_owned());
    let first_difference = difference_detail.map(|observed| OracleFirstDifference {
        checker_id,
        path: "comparison".to_owned(),
        kind: "contract-mismatch".to_owned(),
        row: None,
        expected: "exact comparison succeeded".to_owned(),
        observed,
    });
    let passed = result.is_ok();
    records.push(OracleRecord {
        invariant,
        checker_id,
        operation,
        case_identity: None,
        expected,
        observed,
        oracle_input_digest: input_digest.clone(),
        oracle_observed_digest: observed_digest.clone(),
        oracle_input_bytes: String::new(),
        oracle_observed_bytes: String::new(),
        input_digest,
        observed_digest,
        canonical_version: zeppelin_embed_adversarial_oracle::ORACLE_CONTRACT_VERSION,
        provenance,
        passed,
        first_difference,
        detail: detail.clone(),
    });
    if passed {
        coverage.hit(format!("invariant.I{invariant}.checked"));
    }
}

fn metadata_difference_kind_key(kind: metadata_oracle::DifferenceKind) -> &'static str {
    match kind {
        metadata_oracle::DifferenceKind::ExtraRow => "extra-row",
        metadata_oracle::DifferenceKind::MissingRow => "missing-row",
        metadata_oracle::DifferenceKind::DuplicateRow => "duplicate-row",
        metadata_oracle::DifferenceKind::DeadRow => "dead-row",
        metadata_oracle::DifferenceKind::PrimitiveMismatch => "primitive-mismatch",
        metadata_oracle::DifferenceKind::UnsoundPrune => "unsound-prune",
        metadata_oracle::DifferenceKind::ReportReceiptMismatch => "report-receipt-mismatch",
        metadata_oracle::DifferenceKind::ContractMismatch => "contract-mismatch",
    }
}

fn metadata_first_difference(
    difference: metadata_oracle::FirstDifference,
) -> OracleFirstDifference {
    OracleFirstDifference {
        checker_id: difference.checker_id,
        path: difference.path,
        kind: metadata_difference_kind_key(difference.kind).to_owned(),
        row: difference.row.map(u64::from),
        expected: difference.expected,
        observed: difference.observed,
    }
}

fn vector_first_difference(
    difference: vector_oracle::VectorFirstDifference,
) -> OracleFirstDifference {
    OracleFirstDifference {
        checker_id: difference.checker_id,
        path: difference.path.to_owned(),
        kind: "primitive-mismatch".to_owned(),
        row: None,
        expected: difference.expected,
        observed: difference.observed,
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "vector records bind JSON replay values and family-owned canonical bytes"
)]
fn push_vector_feature_json_record<E: std::fmt::Display>(
    invariant: u8,
    checker_id: &'static str,
    operation: &'static str,
    case_identity: String,
    expected: String,
    observed: String,
    provenance: String,
    attestation: vector_adapter::VectorCanonicalEvidence,
    result: Result<(), E>,
    records: &mut Vec<OracleRecord>,
    coverage: &mut CoverageRegistry,
) {
    let comparison_passed = result.is_ok();
    assert_eq!(
        attestation.checker_id, checker_id,
        "vector family attestation used a different checker"
    );
    assert_eq!(
        attestation.first_difference.is_none(),
        comparison_passed,
        "vector family first difference disagreed with comparator status"
    );
    assert_eq!(
        attestation.input.version,
        vector_oracle::VECTOR_CANONICAL_VERSION,
        "vector input canonical version differs"
    );
    assert_eq!(
        attestation.observed.version,
        vector_oracle::VECTOR_CANONICAL_VERSION,
        "vector observation canonical version differs"
    );
    let position = records.len();
    push_feature_json_record(
        invariant, checker_id, operation, expected, observed, provenance, result, records, coverage,
    );
    let record = records
        .get_mut(position)
        .expect("push_feature_json_record appends exactly one record");
    record.canonical_version = 1;
    record.oracle_input_digest = format!(
        "{}:{}",
        attestation.input.version,
        attestation.input.sha256_hex()
    );
    record.oracle_observed_digest = format!(
        "{}:{}",
        attestation.observed.version,
        attestation.observed.sha256_hex()
    );
    record.oracle_input_bytes = evidence_hex(&attestation.input.bytes);
    record.oracle_observed_bytes = evidence_hex(&attestation.observed.bytes);
    record.case_identity = Some(case_identity);
    record.first_difference = attestation.first_difference.map(vector_first_difference);
}

#[allow(
    clippy::too_many_arguments,
    reason = "metadata records bind JSON replay values and the independent primitive attestation"
)]
fn push_metadata_feature_json_record<E: std::fmt::Display>(
    invariant: u8,
    checker_id: &'static str,
    operation: &'static str,
    expected: String,
    observed: String,
    provenance: String,
    attestation: metadata_oracle::OracleAttestation,
    result: Result<(), E>,
    records: &mut Vec<OracleRecord>,
    coverage: &mut CoverageRegistry,
) {
    let comparison_passed = result.is_ok();
    assert_eq!(
        attestation.checker_id, checker_id,
        "metadata family attestation used a different checker"
    );
    assert_eq!(
        attestation.first_difference.is_none(),
        comparison_passed,
        "metadata family first difference disagreed with comparator status"
    );
    let position = records.len();
    push_feature_json_record(
        invariant, checker_id, operation, expected, observed, provenance, result, records, coverage,
    );
    let record = records
        .get_mut(position)
        .expect("push_feature_json_record appends exactly one record");
    record.canonical_version = attestation.canonical_version;
    record.oracle_input_digest = format!(
        "metadata-v{}:{:016x}",
        attestation.canonical_version, attestation.input_digest
    );
    record.oracle_observed_digest = format!(
        "metadata-v{}:{:016x}",
        attestation.canonical_version, attestation.observed_digest
    );
    record.oracle_input_bytes = evidence_hex(&attestation.input_bytes);
    record.oracle_observed_bytes = evidence_hex(&attestation.observed_bytes);
    record.first_difference = attestation.first_difference.map(metadata_first_difference);
}

const VECTOR_KERNELS: [vector_oracle::KernelId; 11] = [
    vector_oracle::KernelId::DotI8,
    vector_oracle::KernelId::HammingU1,
    vector_oracle::KernelId::DotF32,
    vector_oracle::KernelId::DotF16,
    vector_oracle::KernelId::DotI8Batch,
    vector_oracle::KernelId::HammingU1Batch,
    vector_oracle::KernelId::DotBit4,
    vector_oracle::KernelId::DotBit4Prepared,
    vector_oracle::KernelId::DotBit4Batch,
    vector_oracle::KernelId::ScoreBit4PreparedBatch,
    vector_oracle::KernelId::ScoreBit4Ptrs,
];

fn pack_vector_kernel_rows(dimension: usize, rows: usize, seed: u64) -> Vec<u8> {
    let row_bytes = dimension.div_ceil(2);
    let mut packed = vec![0_u8; row_bytes.saturating_mul(rows)];
    for row in 0..rows {
        for coordinate in 0..dimension {
            let nibble = ((seed as usize + row * 5 + coordinate * 3) % 16) as u8;
            let byte = &mut packed[row * row_bytes + coordinate / 2];
            if coordinate.is_multiple_of(2) {
                *byte |= nibble << 4;
            } else {
                *byte |= nibble;
            }
        }
    }
    packed
}

fn prepare_vector_kernel_query(query: &[i8]) -> Vec<i8> {
    let mut prepared = Vec::with_capacity(query.len());
    for block in query.chunks(32) {
        prepared.extend(block.iter().step_by(2).copied());
        prepared.extend(block.iter().skip(1).step_by(2).copied());
    }
    prepared
}

fn vector_kernel_input(
    case_id: u64,
    backend: vector_oracle::BackendId,
    kernel: vector_oracle::KernelId,
    dimension: usize,
    seed: u64,
) -> vector_oracle::KernelInput {
    let coordinate_query = (0..dimension)
        .map(|index| ((index as i32 * 17 + seed as i32) % 127 - 63) as i8)
        .collect::<Vec<_>>();
    let signed_row = (0..dimension)
        .map(|index| ((index as i32 * 29 - seed as i32) % 127 - 63) as i8)
        .collect::<Vec<_>>();
    let batch_rows = match kernel {
        vector_oracle::KernelId::ScoreBit4Ptrs => 4_usize,
        vector_oracle::KernelId::DotI8Batch
        | vector_oracle::KernelId::HammingU1Batch
        | vector_oracle::KernelId::DotBit4Batch
        | vector_oracle::KernelId::ScoreBit4PreparedBatch => 3_usize,
        _ => 0,
    };
    let bytes_a = (0..dimension)
        .map(|index| (seed as u8).wrapping_add((index * 37) as u8))
        .collect::<Vec<_>>();
    let hamming_rows = (0..dimension.saturating_mul(batch_rows.max(1)))
        .map(|index| (seed as u8).wrapping_add((index * 19) as u8))
        .collect::<Vec<_>>();
    let bit4_rows = pack_vector_kernel_rows(dimension, batch_rows.max(1), seed);
    let f32_a = (0..dimension)
        .map(|index| {
            let sign = if index.is_multiple_of(2) { 1.0 } else { -1.0 };
            vector_oracle::F32::from_float(sign * (1.0 + index as f32 / 31.0))
        })
        .collect::<Vec<_>>();
    let f32_b = (0..dimension)
        .map(|index| {
            let scale = if index % 3 == 0 { 1.0e-3 } else { 1.0 };
            vector_oracle::F32::from_float(scale * (0.5 - index as f32 / 63.0))
        })
        .collect::<Vec<_>>();
    let f16_pattern = [0x0000_u16, 0x3c00, 0xbc00, 0x3555, 0x0400, 0x7bff];
    let f16_a = (0..dimension)
        .map(|index| f16_pattern[index % f16_pattern.len()])
        .collect::<Vec<_>>();
    let f16_b = (0..dimension)
        .map(|index| f16_pattern[(index + 2) % f16_pattern.len()])
        .collect::<Vec<_>>();
    let factors = (0..batch_rows.max(1))
        .map(|row| {
            [
                vector_oracle::F32::from_float(0.5 + row as f32 / 8.0),
                vector_oracle::F32::from_float(1.0 + row as f32 / 4.0),
                vector_oracle::F32::from_float(0.25 + row as f32 / 16.0),
            ]
        })
        .collect::<Vec<_>>();
    let prepared = prepare_vector_kernel_query(&coordinate_query);
    let query_sum = coordinate_query.iter().map(|value| i32::from(*value)).sum();
    let signed_a = match kernel {
        vector_oracle::KernelId::DotBit4Prepared
        | vector_oracle::KernelId::ScoreBit4PreparedBatch
        | vector_oracle::KernelId::ScoreBit4Ptrs => prepared,
        _ => coordinate_query,
    };
    let signed_b = if kernel == vector_oracle::KernelId::DotI8Batch {
        (0..batch_rows)
            .flat_map(|row| {
                signed_row
                    .iter()
                    .map(move |value| value.wrapping_add(row as i8))
            })
            .collect()
    } else {
        signed_row
    };
    let bytes_b = match kernel {
        vector_oracle::KernelId::HammingU1 => hamming_rows[..dimension].to_vec(),
        vector_oracle::KernelId::HammingU1Batch => hamming_rows,
        _ => bit4_rows,
    };
    vector_oracle::KernelInput {
        case_id,
        backend,
        selected_for_store: false,
        work_items: match kernel {
            vector_oracle::KernelId::DotI8Batch
            | vector_oracle::KernelId::HammingU1Batch
            | vector_oracle::KernelId::DotBit4Batch
            | vector_oracle::KernelId::ScoreBit4PreparedBatch
            | vector_oracle::KernelId::ScoreBit4Ptrs => dimension.saturating_mul(batch_rows) as u64,
            _ => dimension as u64,
        },
        kernel,
        dimension: dimension as u64,
        signed_a,
        signed_b,
        bytes_a,
        bytes_b,
        f32_a,
        f32_b,
        f16_a,
        f16_b,
        row_bytes: dimension.div_ceil(2) as u64,
        batch_rows: batch_rows as u64,
        pointer_order: if kernel == vector_oracle::KernelId::ScoreBit4Ptrs {
            vec![2, 0, 3, 1]
        } else {
            Vec::new()
        },
        query_sum,
        query_scale_half: vector_oracle::F64::from_float(0.5),
        bit4_factors: factors,
    }
}

fn observe_vector_kernel(
    variant: KernelVariant,
    input: &vector_oracle::KernelInput,
) -> Result<vector_oracle::KernelValue, String> {
    let dimension = usize::try_from(input.dimension).map_err(|_| "kernel dimension overflow")?;
    let rows = usize::try_from(input.batch_rows).map_err(|_| "kernel row count overflow")?;
    match input.kernel {
        vector_oracle::KernelId::DotI8 => Ok(vector_oracle::KernelValue::S32(
            variant.dot_i8(&input.signed_a, &input.signed_b),
        )),
        vector_oracle::KernelId::HammingU1 => Ok(vector_oracle::KernelValue::U32(
            variant.hamming_u1(&input.bytes_a, &input.bytes_b),
        )),
        vector_oracle::KernelId::DotF32 => Ok(vector_oracle::KernelValue::F32(
            vector_oracle::F32::from_float(
                variant.dot_f32(
                    &input
                        .f32_a
                        .iter()
                        .map(|value| value.to_float())
                        .collect::<Vec<_>>(),
                    &input
                        .f32_b
                        .iter()
                        .map(|value| value.to_float())
                        .collect::<Vec<_>>(),
                ),
            ),
        )),
        vector_oracle::KernelId::DotF16 => Ok(vector_oracle::KernelValue::F32(
            vector_oracle::F32::from_float(variant.dot_f16(&input.f16_a, &input.f16_b)),
        )),
        vector_oracle::KernelId::DotI8Batch => {
            let mut out = vec![0_i32; rows];
            variant.dot_i8_batch(&input.signed_a, &input.signed_b, dimension, &mut out);
            Ok(vector_oracle::KernelValue::S32s(out))
        }
        vector_oracle::KernelId::HammingU1Batch => {
            let mut out = vec![0_u32; rows];
            variant.hamming_u1_batch(&input.bytes_a, &input.bytes_b, dimension, &mut out);
            Ok(vector_oracle::KernelValue::U32s(out))
        }
        vector_oracle::KernelId::DotBit4 => Ok(vector_oracle::KernelValue::S32(
            variant.dot_bit4(&input.signed_a, &input.bytes_b),
        )),
        vector_oracle::KernelId::DotBit4Prepared => Ok(vector_oracle::KernelValue::S32(
            variant.dot_bit4_prepared(&input.signed_a, input.query_sum, &input.bytes_b),
        )),
        vector_oracle::KernelId::DotBit4Batch => {
            let mut out = vec![0_i32; rows];
            variant.dot_bit4_batch(&input.signed_a, &input.bytes_b, dimension, &mut out);
            Ok(vector_oracle::KernelValue::S32s(out))
        }
        vector_oracle::KernelId::ScoreBit4PreparedBatch => {
            let factors = input
                .bit4_factors
                .iter()
                .map(|fields| {
                    Bit4Factors::from_persisted(
                        fields[0].to_float(),
                        fields[1].to_float(),
                        fields[2].to_float(),
                    )
                })
                .collect::<Vec<_>>();
            let mut out = vec![0.0_f32; rows];
            variant.score_bit4_prepared_batch(
                (
                    &input.signed_a,
                    input.query_sum,
                    input.query_scale_half.to_float(),
                ),
                &input.bytes_b,
                dimension,
                &factors,
                &mut out,
            );
            Ok(vector_oracle::KernelValue::F32s(
                out.into_iter()
                    .map(vector_oracle::F32::from_float)
                    .collect(),
            ))
        }
        vector_oracle::KernelId::ScoreBit4Ptrs => {
            let row_bytes = dimension.div_ceil(2);
            let order = input
                .pointer_order
                .iter()
                .map(|row| usize::from(*row))
                .collect::<Vec<_>>();
            let row_handles = std::array::from_fn(|slot| {
                zeppelin_embed::kernels::Bit4Row::from_mapped_region(
                    &input.bytes_b,
                    order[slot] * row_bytes,
                    row_bytes,
                )
                .expect("oracle-validated pointer row")
            });
            let rows4 = zeppelin_embed::kernels::Bit4Rows4::from_rows(row_handles, row_bytes)
                .map_err(|error| error.to_string())?;
            let factors = std::array::from_fn(|slot| {
                let fields = input.bit4_factors[order[slot]];
                Bit4Factors::from_persisted(
                    fields[0].to_float(),
                    fields[1].to_float(),
                    fields[2].to_float(),
                )
            });
            let mut out = [0.0_f32; 4];
            variant
                .score_bit4_ptrs(
                    (
                        &input.signed_a,
                        input.query_sum,
                        input.query_scale_half.to_float(),
                    ),
                    &rows4,
                    &factors,
                    &mut out,
                )
                .map_err(|error| error.to_string())?;
            Ok(vector_oracle::KernelValue::F32s(
                out.into_iter()
                    .map(vector_oracle::F32::from_float)
                    .collect(),
            ))
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "one operation owns exact replay evidence and receipt streams"
)]
fn run_vector_kernel_parity(
    selected_faults: &[super::campaign::FeatureFault],
    seed: u64,
    profile: FaultProfile,
    op_index: usize,
    oracle_records: &mut Vec<OracleRecord>,
    control_records: &mut Vec<String>,
    mutation_records: &mut Vec<String>,
    coverage: &mut CoverageRegistry,
) -> Result<Vec<ProductionFeatureReceipt>, String> {
    for fault in selected_faults {
        if *fault != super::campaign::FeatureFault::VectorForcedDispatchBackend {
            return Err(format!(
                "vector kernel operation received unrelated fault {}",
                fault.key()
            ));
        }
    }

    let dimensions = [
        1_usize, 2, 3, 7, 15, 16, 31, 32, 33, 63, 64, 65, 127, 128, 129, 768,
    ];
    let dimension = dimensions[(seed as usize) % dimensions.len()];
    let mut available = Vec::new();
    for (backend_index, variant) in KernelVariant::available().enumerate() {
        let backend = vector_backend_id(variant.backend_id());
        for (kernel_index, kernel) in VECTOR_KERNELS.into_iter().enumerate() {
            let case_id = seed
                .checked_mul(100_000)
                .and_then(|value| value.checked_add((backend_index * VECTOR_KERNELS.len()) as u64))
                .and_then(|value| value.checked_add(kernel_index as u64))
                .ok_or_else(|| "vector kernel case id overflowed".to_owned())?;
            let input = vector_kernel_input(case_id, backend, kernel, dimension, seed);
            let expected = vector_oracle::expected_kernel(&input)?;
            let observed = vector_oracle::I24Observed {
                case_id,
                backend,
                kernel,
                value: observe_vector_kernel(variant, &input)?,
                selected_for_store: false,
                work_items: input.work_items,
            };
            push_feature_record(
                24,
                vector_oracle::I24_CHECKER_ID,
                "kernel-parity",
                &expected,
                &observed,
                format!(
                    "{} seed={seed} profile={} op={op_index} backend={} kernel={kernel:?}",
                    vector_oracle::VECTOR_ORACLE_CONTRACT,
                    profile.key(),
                    variant.backend_id().as_str()
                ),
                vector_oracle::check_i24(&expected, &observed),
                oracle_records,
                coverage,
            );
        }
        coverage.hit(format!("kernel.backend.{}", variant.backend_id().as_str()));
        coverage.hit(format!(
            "kernel.backend.{}",
            format!("{:?}", variant.arm()).to_ascii_lowercase()
        ));
        available.push(variant.backend_id());
    }
    if available.is_empty() {
        return Err("vector kernel inventory was empty".to_owned());
    }

    let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
    let controller = KernelFaultController::observing_store(seed);
    let dependencies = StoreTestDependencies::new(Arc::new(StdVfs), Arc::new(SystemMonotonicClock))
        .with_kernel_fault_controller(controller.clone());
    let store =
        Store::open_with_test_dependencies(directory.path(), OpenOptions::default(), dependencies)
            .map_err(|error| error.to_string())?;
    let documents = [
        IngestDocument::new(
            DocumentVersion::new(DocId::new(100), Revision::new(1)),
            vec![1.0, -1.0, 0.5],
        ),
        IngestDocument::new(
            DocumentVersion::new(DocId::new(200), Revision::new(1)),
            vec![0.5, -0.5, 0.25],
        ),
    ];
    store
        .ingest(IngestBatch::new(documents.to_vec()))
        .map_err(|error| error.to_string())?;
    store.seal().map_err(|error| error.to_string())?;
    let outcome = store
        .search(
            SearchRequest::new(&[1.0, -1.0, 0.5]),
            documents.len(),
            SearchOptions::new(ScanOptions { thread_budget: 1 }).with_tier(SearchTier::Scan),
            QueryControl::Cancel(CancelToken::new()),
        )
        .map_err(|error| error.to_string())?;
    let observations = controller.take_observations();
    if observations.len() != 1 {
        return Err(format!(
            "I24 default Store selection emitted {} observations, expected 1",
            observations.len()
        ));
    }
    let selected = &observations[0];
    if !available.contains(&selected.backend())
        || !selected.result_published()
        || selected.work_items() == 0
        || outcome.candidates.is_empty()
    {
        return Err(format!(
            "I24 Store selection was not a published available-table score: {selected:?}"
        ));
    }
    if !controller.take_typed_receipts().is_empty() {
        return Err("neutral I24 Store observation emitted a fault receipt".to_owned());
    }
    control_records.push(format!(
        "{{\"campaign\":\"vector-execution\",\"operation\":\"kernel-parity\",\"seed\":{seed},\"backend\":\"{}\",\"kernel\":\"{:?}\",\"work_items\":{},\"result\":\"{:?}\",\"returned\":{}}}",
        selected.backend().as_str(),
        selected.kernel(),
        selected.work_items(),
        selected.value(),
        outcome.candidates.len()
    ));
    mutation_records.push(format!(
        "{{\"campaign\":\"vector-execution\",\"operation\":\"kernel-parity\",\"seed\":{seed},\"dimension\":{dimension},\"backend_count\":{}}}",
        available.len()
    ));
    coverage.hit("op.search");
    coverage.hit("search.scan");

    if selected_faults.is_empty() {
        return Ok(Vec::new());
    }
    Err("forced-dispatch-backend requires the fresh-child adapter".to_owned())
}

fn run_fts_region_probe(fault: super::campaign::FeatureFault) -> Result<(), String> {
    let directory = tempfile::tempdir().map_err(|e| e.to_string())?;
    let mut engine = RealEngine::without_faults(directory.path().to_path_buf());
    engine.open()?;
    engine.ingest(&[DocMutation {
        doc_id: 1,
        revision: 1,
        timestamp: 1,
    }])?;
    engine.seal()?;
    engine.close()?;
    let segment = first_segment_path(directory.path())?;
    let mut bytes = std::fs::read(&segment).map_err(|e| e.to_string())?;
    let kind = match fault {
        super::campaign::FeatureFault::FtsStoredTextCorruption
        | super::campaign::FeatureFault::FtsStoredTextAbsence => 14,
        super::campaign::FeatureFault::FtsDictionaryCorruption => 6,
        super::campaign::FeatureFault::FtsNormCorruption => 6,
        super::campaign::FeatureFault::FtsBlockMaxCorruption => 6,
        _ => 6,
    };
    let offset = literal_segment_region_offset(&bytes, kind)?;
    if let Some(byte) = bytes.get_mut(offset) {
        *byte ^= 0x5a;
    } else {
        return Err("fts mutation escaped segment".into());
    }
    std::fs::write(&segment, bytes).map_err(|e| e.to_string())?;
    let mut reopened = RealEngine::without_faults(directory.path().to_path_buf());
    reopened.open()?;
    let _result = reopened.lexical_search(0, 8)?;
    let _ = reopened.close();
    Err("legacy FTS fault probe cannot earn feature credit".to_owned())
}

fn run_fts_cancellation_probe() -> Result<(), String> {
    let directory = tempfile::tempdir().map_err(|e| e.to_string())?;
    let mut engine = RealEngine::without_faults(directory.path().to_path_buf());
    engine.open()?;
    engine.ingest(&[DocMutation {
        doc_id: 1,
        revision: 1,
        timestamp: 1,
    }])?;
    let store = engine.store()?;
    let query = TermQuery::flat(vec![program::lexical_query(0).to_vec()], &[DEFAULT_FIELD]);
    let cancel = CancelToken::new();
    cancel.cancel();
    let result = store.search_lexical(&query, 8, QueryControl::Cancel(cancel));
    let _ = engine.close();
    match result {
        Err(_) => Ok(()),
        Ok(_) => Err("lexical cancellation was ignored".to_owned()),
    }
}

fn run_hybrid_leg_panic_probe() -> Result<(), String> {
    let directory = tempfile::tempdir().map_err(|e| e.to_string())?;
    let dependencies = StoreTestDependencies::new(Arc::new(StdVfs), Arc::new(SystemMonotonicClock))
        .with_hybrid_leg_fault(HybridLegTestFault::Panic(FusionLeg::Lexical));
    let store =
        Store::open_with_test_dependencies(directory.path(), OpenOptions::default(), dependencies)
            .map_err(|e| e.to_string())?;
    let document = IngestDocument::new(
        DocumentVersion::new(DocId::new(1), Revision::new(1)),
        program::vector(1, 1).to_vec(),
    )
    .with_text("hybrid panic probe");
    store
        .ingest(IngestBatch::new(vec![document]))
        .map_err(|e| e.to_string())?;
    let term = TermQuery::flat(vec![b"hybrid".to_vec()], &[DEFAULT_FIELD]);
    let result = store.search_hybrid(
        SearchRequest::new(&program::query(0)),
        &term,
        &HybridQuery::new(1),
        SearchOptions::default(),
        QueryControl::Cancel(CancelToken::new()),
    );
    let _ = store.close();
    match result {
        Err(error) if error.to_string().contains("panicked") => Ok(()),
        Err(_) => Ok(()),
        Ok(_) => Err("injected hybrid leg panic was not contained".to_owned()),
    }
}

fn run_hybrid_fault_probe(fault: super::campaign::FeatureFault) -> Result<(), String> {
    let query = HybridQuery::new(1);
    let result = match fault {
        super::campaign::FeatureFault::HybridVectorLegError => execute_hybrid(
            &query,
            || {
                Err(FusionError::Leg {
                    leg: FusionLeg::Vector,
                    kind: zeppelin_embed::fusion::LegFailureKind::Caller,
                    detail: "injected vector leg failure".to_owned(),
                })
            },
            || Ok(vec![LexicalCandidate::new(1_u32, 1.0)]),
            |value| Some(*value),
            |value| Some(*value),
        ),
        super::campaign::FeatureFault::HybridLexicalLegError => execute_hybrid(
            &query,
            || Ok(vec![VectorCandidate::exact(1_u32, 0.0)]),
            || {
                Err(FusionError::Leg {
                    leg: FusionLeg::Lexical,
                    kind: zeppelin_embed::fusion::LegFailureKind::Caller,
                    detail: "injected lexical leg failure".to_owned(),
                })
            },
            |value| Some(*value),
            |value| Some(*value),
        ),
        super::campaign::FeatureFault::HybridDualFailureOrder => execute_hybrid(
            &query,
            || {
                Err(FusionError::Leg {
                    leg: FusionLeg::Vector,
                    kind: zeppelin_embed::fusion::LegFailureKind::Caller,
                    detail: "injected vector failure".to_owned(),
                })
            },
            || {
                Err(FusionError::Leg {
                    leg: FusionLeg::Lexical,
                    kind: zeppelin_embed::fusion::LegFailureKind::Caller,
                    detail: "injected lexical failure".to_owned(),
                })
            },
            |value| Some(*value),
            |value| Some(*value),
        ),
        super::campaign::FeatureFault::HybridEstimatedScore => execute_hybrid(
            &query,
            || Ok(vec![VectorCandidate::estimated(1_u32, 0.5)]),
            || Ok(vec![LexicalCandidate::new(1_u32, 1.0)]),
            |value| Some(*value),
            |value| Some(*value),
        ),
        super::campaign::FeatureFault::HybridNonfiniteScore => execute_hybrid(
            &query,
            || Ok(vec![VectorCandidate::exact(1_u32, f64::NAN)]),
            || Ok(vec![LexicalCandidate::new(1_u32, 1.0)]),
            |value| Some(*value),
            |value| Some(*value),
        ),
        _ => return Err("wrong hybrid fault routed to pure fusion probe".to_owned()),
    };
    match result {
        Err(_) => Ok(()),
        Ok(_) => Err(format!("hybrid fault {} was accepted", fault.key())),
    }
}

fn run_hybrid_cancellation_probe() -> Result<(), String> {
    let directory = tempfile::tempdir().map_err(|e| e.to_string())?;
    let store = Store::open(directory.path(), RealEngine::options(ModelEpoch::A))
        .map_err(|e| e.to_string())?;
    let term = TermQuery::flat(vec![b"cancel".to_vec()], &[DEFAULT_FIELD]);
    let token = CancelToken::new();
    token.cancel();
    let result = store.search_hybrid(
        SearchRequest::new(&program::query(0)),
        &term,
        &HybridQuery::new(1),
        SearchOptions::default(),
        QueryControl::Cancel(token),
    );
    let _ = store.close();
    match result {
        Err(_) => Ok(()),
        Ok(_) => Err("hybrid cancellation was ignored".to_owned()),
    }
}

fn run_lock_contention_probe() -> Result<(), String> {
    let directory = tempfile::tempdir().map_err(|e| e.to_string())?;
    let first = Store::open(directory.path(), RealEngine::options(ModelEpoch::A))
        .map_err(|e| e.to_string())?;
    let second = Store::open(directory.path(), RealEngine::options(ModelEpoch::A));
    if second.is_ok() {
        return Err("conflicting writer admission was not rejected".to_owned());
    }
    first.close().map_err(|e| e.to_string())
}

fn run_health_reopen_probe() -> Result<(), String> {
    let directory = tempfile::tempdir().map_err(|e| e.to_string())?;
    let mut engine = RealEngine::without_faults(directory.path().to_path_buf());
    engine.open()?;
    engine.ingest(&[DocMutation {
        doc_id: 1,
        revision: 1,
        timestamp: 1,
    }])?;
    engine.close()?;
    let mut reopened = RealEngine::without_faults(directory.path().to_path_buf());
    reopened.open()?;
    let _ = reopened.search(&program::query(0), 1, SearchKind::Scan, 0)?;
    reopened.close()
}

fn run_ffi_fault_probe(fault: super::campaign::FeatureFault) -> Result<(), String> {
    use zeppelin_embed_ffi::{ZeErrorCode, ZeHandle, ze_close, ze_error_code_name, ze_open};
    let mut handle: ZeHandle = 0;
    let code = ze_open(std::ptr::null(), &mut handle);
    if code != ZeErrorCode::ZeErrInvalidArgument {
        return Err(format!(
            "FFI {} returned {:?} for null request",
            fault.key(),
            code
        ));
    }
    let stale = ze_close(0);
    if stale != ZeErrorCode::ZeErrInvalidHandle {
        return Err(format!(
            "FFI {} stale handle returned {:?}",
            fault.key(),
            stale
        ));
    }
    let name = ze_error_code_name(code as i32);
    if name.is_null() {
        return Err("FFI error code name returned null".to_owned());
    }
    Ok(())
}

fn run_graph_checkpoint_corruption_probe() -> Result<(), String> {
    for bytes in [Vec::new(), vec![0_u8; 128]] {
        if zeppelin_embed::graph::build::validate_graph_build_checkpoint(&bytes).is_ok() {
            return Err("corrupt graph checkpoint was accepted".to_owned());
        }
    }
    Ok(())
}

fn graph_probe_engine() -> Result<(TempDir, RealEngine), String> {
    let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
    let mut engine = RealEngine::without_faults(directory.path().to_path_buf());
    engine.open()?;
    let documents = (1..=program::GRAPH_ROWS)
        .map(|doc_id| DocMutation {
            doc_id,
            revision: 1,
            timestamp: 10,
        })
        .collect::<Vec<_>>();
    engine.ingest(&documents)?;
    engine.seal()?;
    let _ = engine.maintain(u64::MAX)?;
    Ok((directory, engine))
}

fn run_graph_missing_rescore_probe() -> Result<(), String> {
    let (directory, mut engine) = graph_probe_engine()?;
    engine.close()?;
    let segment = first_segment_path(directory.path())?;
    let mut bytes = std::fs::read(&segment).map_err(|error| error.to_string())?;
    let offset = literal_segment_region_offset(&bytes, 5)?;
    let byte = bytes
        .get_mut(offset)
        .ok_or_else(|| "rescore mutation escaped the segment".to_owned())?;
    *byte ^= 0x01;
    std::fs::write(&segment, bytes).map_err(|error| error.to_string())?;
    let mut reopened = RealEngine::without_faults(directory.path().to_path_buf());
    reopened.open()?;
    let result = reopened.search(&program::query(1), 8, SearchKind::Graph, 0);
    match result {
        Err(_) => {
            let _ = reopened.close();
            Ok(())
        }
        Ok(observed) if !observed.hits.is_empty() => reopened.close(),
        Ok(_) => {
            let _ = reopened.close();
            Err("missing graph rescore returned no candidates".to_owned())
        }
    }
}

fn run_graph_search_cancellation_probe() -> Result<(), String> {
    let (_directory, mut engine) = graph_probe_engine()?;
    let token = CancelToken::new();
    token.cancel();
    let result = engine.store()?.search(
        SearchRequest::new(&program::query(1)),
        8,
        SearchOptions::new(ScanOptions {
            thread_budget: THREAD_BUDGET,
        })
        .with_tier(SearchTier::Graph(GraphSearchOptions::new(
            GraphSearchProfile::SiftClass,
        ))),
        QueryControl::Cancel(token),
    );
    let _ = engine.close();
    match result {
        Err(_) => Ok(()),
        Ok(_) => Err("cancelled graph search returned a successful result".to_owned()),
    }
}

fn run_graph_corruption_probe(fault: super::campaign::FeatureFault) -> Result<(), String> {
    let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
    let mut engine = RealEngine::without_faults(directory.path().to_path_buf());
    engine.open()?;
    let documents = (1..=program::GRAPH_ROWS)
        .map(|doc_id| DocMutation {
            doc_id,
            revision: 1,
            timestamp: 10,
        })
        .collect::<Vec<_>>();
    engine.ingest(&documents)?;
    engine.seal()?;
    let _ = engine.maintain(u64::MAX)?;
    engine.close()?;
    let segment = first_segment_path(directory.path())?;
    let mut bytes = std::fs::read(&segment).map_err(|error| error.to_string())?;
    let offset = literal_segment_region_offset(&bytes, 7)?;
    let target = if fault == super::campaign::FeatureFault::GraphCorruptEntry {
        offset
    } else {
        offset.saturating_add(16)
    };
    let byte = bytes
        .get_mut(target)
        .ok_or_else(|| format!("graph {fault:?} mutation escaped the region"))?;
    *byte ^= 0xff;
    std::fs::write(&segment, bytes).map_err(|error| error.to_string())?;
    let mut reopened = RealEngine::without_faults(directory.path().to_path_buf());
    reopened.open()?;
    let result = reopened.search(&program::query(1), 8, SearchKind::Graph, 0);
    match result {
        Err(_) => {
            let _ = reopened.close();
            Ok(())
        }
        Ok(observed) => {
            let invalid = observed.hits.iter().any(|hit| hit.doc_id == 0);
            let _ = reopened.close();
            if invalid {
                Err(format!(
                    "graph corruption {} returned an invalid row",
                    fault.key()
                ))
            } else {
                Ok(())
            }
        }
    }
}

fn run_graph_budget_cancel_probe() -> Result<(), String> {
    let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
    let mut engine = RealEngine::without_faults(directory.path().to_path_buf());
    engine.open()?;
    let documents = (1..=program::GRAPH_ROWS)
        .map(|doc_id| DocMutation {
            doc_id,
            revision: 1,
            timestamp: 10,
        })
        .collect::<Vec<_>>();
    engine.ingest(&documents)?;
    let deferred = engine.maintain(0);
    if let Err(error) = deferred
        && !error.contains("budget")
        && !error.contains("defer")
    {
        return Err(format!(
            "zero-budget graph build returned wrong error: {error}"
        ));
    }
    let _completed = engine.maintain(u64::MAX)?;
    let observed = engine.search(&program::query(1), 8, SearchKind::Auto, 0)?;
    if observed.hits.is_empty() {
        return Err("completed graph-budget probe returned no searchable rows".to_owned());
    }
    engine.close()
}

fn run_forced_backend_fault_probe() -> Result<(), String> {
    let mut receipt_coverage = CoverageRegistry::default();
    run_kernel_parity_probe(&mut receipt_coverage)?;
    if receipt_coverage.count("kernel.backend.scalar") != 1 {
        return Err("forced backend probe did not execute the scalar production kernel".to_owned());
    }
    Ok(())
}

fn run_corrupt_codes_factors_fault_probe() -> Result<(), String> {
    let query = prepare_bit4_query(&[1.0, -2.0, 0.5], 17).map_err(|error| error.to_string())?;
    let mut codes = [0_u8; 2];
    let factors =
        quantize_bit4(&[0.25, -1.5, 2.0], &mut codes).map_err(|error| error.to_string())?;
    codes[1] |= 0x0f;
    if !matches!(
        est_dot_bit4(&query, &codes, factors),
        Err(QuantError::NonZeroPadding { .. })
    ) {
        return Err("corrupted packed code was not rejected as non-canonical padding".to_owned());
    }

    let mut output = [0xaa_u8; 2];
    let before = output;
    let error = quantize_bit4(&[1.0, f32::NAN, 2.0], &mut output)
        .expect_err("non-finite vector unexpectedly produced persisted factors");
    if error != (QuantError::NonFinite { index: 1 }) || output != before {
        return Err(format!(
            "non-finite quantization did not preserve the caller buffer: error={error}, before={before:?}, after={output:?}"
        ));
    }
    Ok(())
}

fn run_missing_rescore_rows_fault_probe() -> Result<(), String> {
    let query = [0.0_f32, 0.0];
    let rows = [100.0_f32, 0.0, 2.0, 0.0, 0.0, 2.0];
    let row_indices = [0_u32, 3, 1];
    let coarse_scores = [10.0_f32, 9.0, 8.0];
    let error = rescore_top_k(
        &query,
        &rows,
        2,
        RescorePool::retained(&row_indices, &coarse_scores, RescoreMetric::SquaredL2, 3, 1),
        2,
    )
    .expect_err("missing full-precision row unexpectedly entered exact top-k");
    if error
        != (RescoreError::CandidateRowOutOfRange {
            position: 1,
            row_index: 3,
            row_count: 3,
        })
    {
        return Err(format!("missing rescore row returned wrong error: {error}"));
    }
    Ok(())
}

fn run_vector_cancellation_fault_probe() -> Result<(), String> {
    let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
    let store = Store::open(directory.path(), RealEngine::options(ModelEpoch::A))
        .map_err(|error| error.to_string())?;
    let documents = ingest_probe_batch()
        .into_iter()
        .map(|document| {
            IngestDocument::new(
                DocumentVersion::new(
                    DocId::new(u128::from(document.doc_id)),
                    Revision::new(document.revision),
                ),
                program::vector(document.doc_id, document.revision).to_vec(),
            )
            .with_timestamp(document.timestamp)
            .with_text(program::lexical_text(document.doc_id, document.revision))
            .with_columns(adversarial_columns(document.doc_id))
        })
        .collect::<Vec<_>>();
    store
        .ingest(IngestBatch::new(documents).with_epoch(declared_identity()))
        .map_err(|error| error.to_string())?;
    let cancel = CancelToken::new();
    cancel.cancel();
    let error = store
        .search(
            SearchRequest::new(&program::query(3)),
            3,
            SearchOptions::new(ScanOptions {
                thread_budget: THREAD_BUDGET,
            })
            .with_tier(SearchTier::Scan),
            QueryControl::Cancel(cancel),
        )
        .expect_err("cancelled vector scan unexpectedly returned a result buffer");
    if !matches!(
        error,
        zeppelin_embed::lifecycle::QueryError::Cancelled { partial: false }
    ) {
        return Err(format!("vector cancellation returned wrong error: {error}"));
    }
    let clean = store
        .search(
            SearchRequest::new(&program::query(3)),
            3,
            SearchOptions::new(ScanOptions {
                thread_budget: THREAD_BUDGET,
            })
            .with_tier(SearchTier::Scan),
            QueryControl::Cancel(CancelToken::new()),
        )
        .map_err(|error| error.to_string())?;
    let identities = clean
        .candidates
        .iter()
        .map(|candidate| candidate.document())
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| "clean scan returned a row without document identity".to_owned())?;
    if identities.len() != 3 {
        return Err(format!(
            "post-cancellation clean scan returned {} rows, expected 3",
            identities.len()
        ));
    }
    store.close().map_err(|error| error.to_string())
}

fn run_vector_allocation_denial_fault_probe() -> Result<(), String> {
    let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
    let mut engine = RealEngine::without_faults(directory.path().to_path_buf());
    engine.open()?;
    engine.ingest(&[ingest_probe_batch()[0]])?;
    engine.seal()?;
    engine.close()?;

    let denied = Store::open(
        directory.path(),
        RealEngine::options(ModelEpoch::A).with_max_resident_bytes(0),
    );
    if !matches!(
        denied,
        Err(zeppelin_embed::lifecycle::StoreError::BudgetExceeded {
            component: "snapshot",
            ..
        })
    ) {
        return Err(format!(
            "snapshot allocation denial returned wrong result: {:?}",
            denied.as_ref().err()
        ));
    }
    let reopened = Store::open(directory.path(), RealEngine::options(ModelEpoch::A))
        .map_err(|error| error.to_string())?;
    reopened
        .snapshot()
        .map_err(|error| format!("clean open after allocation denial was unusable: {error}"))?;
    reopened.close().map_err(|error| error.to_string())
}

fn ingest_probe_batch() -> [DocMutation; 3] {
    [
        DocMutation {
            doc_id: 1,
            revision: 1,
            timestamp: 9,
        },
        DocMutation {
            doc_id: 2,
            revision: 1,
            timestamp: 10,
        },
        DocMutation {
            doc_id: 3,
            revision: 1,
            timestamp: 11,
        },
    ]
}

fn run_wal_damage_fault_probe(fault: super::campaign::FeatureFault) -> Result<(), String> {
    let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
    let mut engine = RealEngine::without_faults(directory.path().to_path_buf());
    engine.open()?;
    engine.ingest(&[DocMutation {
        doc_id: 1,
        revision: 1,
        timestamp: 10,
    }])?;
    engine.close()?;
    let path = directory.path().join("wal.ze");
    let mut bytes = std::fs::read(&path).map_err(|error| error.to_string())?;
    match fault {
        super::campaign::FeatureFault::StorageTornWalHeader => {
            let first = bytes
                .first_mut()
                .ok_or_else(|| "WAL damage probe found an empty WAL".to_owned())?;
            *first ^= 0x80;
        }
        super::campaign::FeatureFault::StorageTornWalBody => {
            if bytes.len() <= 41 {
                return Err(format!(
                    "WAL body probe needs a record after the 40-byte header, got {} bytes",
                    bytes.len()
                ));
            }
            let keep = 40_usize.saturating_add((bytes.len() - 40) / 2);
            bytes.truncate(keep);
        }
        super::campaign::FeatureFault::StorageTornWalChecksum => {
            let last = bytes
                .last_mut()
                .ok_or_else(|| "WAL checksum probe found an empty WAL".to_owned())?;
            *last ^= 0x01;
        }
        _ => return Err("non-WAL fault reached the WAL damage probe".to_owned()),
    }
    std::fs::write(&path, bytes).map_err(|error| error.to_string())?;
    let mut reopened = RealEngine::without_faults(directory.path().to_path_buf());
    match reopened.open() {
        Err(_) => Ok(()),
        Ok(()) => {
            let result = reopened.search(&program::query(3), 8, SearchKind::Scan, 0);
            let _ = reopened.close();
            match result {
                Err(_) => Ok(()),
                Ok(observed) => Err(format!(
                    "damaged WAL was consumed as a healthy store with ids {:?}",
                    observed
                        .hits
                        .iter()
                        .map(|hit| hit.doc_id)
                        .collect::<Vec<_>>()
                )),
            }
        }
    }
}

fn run_post_commit_retry_fault_probe() -> Result<(), String> {
    let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
    let event = FaultEvent {
        id: "feature-storage-post-commit".to_owned(),
        op_index: 1,
        layer: fault_vfs::Layer::Io,
        site: fault_vfs::FaultSite::Append,
        mode: fault_vfs::FaultMode::PostCommitError,
        nth_match: 1,
        expected_matches: None,
        deadline_budget_seconds: None,
        path_contains: Some("wal.ze".to_owned()),
        fired: false,
        fire_count: 0,
        path: None,
    };
    let scheduled = Arc::new(fault_vfs::simulated_scheduled(
        fault_vfs::FaultSchedule::single(event),
    ));
    let mut engine = RealEngine::new(
        directory.path().to_path_buf(),
        Arc::clone(&scheduled),
        Arc::new(ManualMonotonicClock::new()),
    );
    scheduled.set_operation(0);
    engine.open()?;
    scheduled.set_operation(1);
    let document = DocMutation {
        doc_id: 1,
        revision: 1,
        timestamp: 10,
    };
    let _ambiguous = engine.ingest(&[document]);
    if !scheduled.events().into_iter().any(|event| event.fired) {
        return Err("post-commit append fault did not reach the Store WAL".to_owned());
    }
    engine.reopen()?;
    let _retry = engine.ingest(&[document]);
    let observed = engine.search(&program::query(3), 8, SearchKind::Scan, 0)?;
    let matching = observed
        .hits
        .iter()
        .filter(|hit| hit.doc_id == document.doc_id && hit.revision == document.revision)
        .count();
    if matching != 1 {
        return Err(format!(
            "ambiguous retry produced {matching} logical copies instead of one"
        ));
    }
    engine.close()
}

fn run_persisted_object_fault_probe(fault: super::campaign::FeatureFault) -> Result<(), String> {
    let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
    let mut engine = RealEngine::without_faults(directory.path().to_path_buf());
    engine.open()?;
    engine.ingest(&[DocMutation {
        doc_id: 1,
        revision: 1,
        timestamp: 10,
    }])?;
    engine.seal()?;
    engine.close()?;
    let manifest = directory.path().join("manifest.ze");
    let segment = first_segment_path(directory.path())?;
    let manifest_bytes = std::fs::read(&manifest).map_err(|error| error.to_string())?;
    let segment_bytes = std::fs::read(&segment).map_err(|error| error.to_string())?;
    match fault {
        super::campaign::FeatureFault::StorageCorruptSegmentRegion
        | super::campaign::FeatureFault::TierStaleSource => {
            let mut damaged = segment_bytes;
            let offset = literal_segment_region_offset(&damaged, 3)?;
            let byte = damaged
                .get_mut(offset)
                .ok_or_else(|| "vector-code mutation escaped the segment".to_owned())?;
            *byte ^= 0x01;
            std::fs::write(&segment, damaged).map_err(|error| error.to_string())?;
        }
        super::campaign::FeatureFault::StorageWrongManifestObject => {
            std::fs::write(&manifest, segment_bytes).map_err(|error| error.to_string())?;
        }
        super::campaign::FeatureFault::StorageWrongSegmentObject => {
            std::fs::write(&segment, manifest_bytes).map_err(|error| error.to_string())?;
        }
        _ => return Err("non-object fault reached persisted-object probe".to_owned()),
    }
    let mut reopened = RealEngine::without_faults(directory.path().to_path_buf());
    match reopened.open() {
        Err(_) => Ok(()),
        Ok(()) => {
            let result = reopened.search(&program::query(3), 8, SearchKind::Scan, 0);
            let _ = reopened.close();
            match result {
                Err(_) => Ok(()),
                Ok(_) => Err(format!(
                    "persisted object fault {} was consumed successfully",
                    fault.key()
                )),
            }
        }
    }
}

fn literal_segment_region_offset(bytes: &[u8], target_kind: u16) -> Result<usize, String> {
    const FILE_HEADER_BYTES: usize = 32;
    const SEGMENT_PREFIX_BYTES: usize = 32;
    const REGION_ENTRY_BYTES: usize = 32;
    let region_count = read_literal_u16(bytes, FILE_HEADER_BYTES + 20)? as usize;
    let directory = FILE_HEADER_BYTES + SEGMENT_PREFIX_BYTES;
    for position in 0..region_count {
        let entry = directory
            .checked_add(position.saturating_mul(REGION_ENTRY_BYTES))
            .ok_or_else(|| "segment directory offset overflow".to_owned())?;
        if read_literal_u16(bytes, entry)? != target_kind {
            continue;
        }
        let offset = usize::try_from(read_literal_u64(bytes, entry + 8)?)
            .map_err(|_| "segment region offset exceeds usize".to_owned())?;
        let length = usize::try_from(read_literal_u64(bytes, entry + 16)?)
            .map_err(|_| "segment region length exceeds usize".to_owned())?;
        if length == 0
            || offset
                .checked_add(length)
                .is_none_or(|end| end > bytes.len())
        {
            return Err(format!(
                "guarded region {target_kind} has invalid offset/length {offset}/{length}"
            ));
        }
        return Ok(offset + length / 2);
    }
    Err(format!("segment omitted target region kind {target_kind}"))
}

fn read_literal_u16(bytes: &[u8], offset: usize) -> Result<u16, String> {
    bytes
        .get(offset..offset.saturating_add(2))
        .and_then(|slice| slice.try_into().ok())
        .map(u16::from_le_bytes)
        .ok_or_else(|| format!("missing u16 at segment offset {offset}"))
}

fn read_literal_u64(bytes: &[u8], offset: usize) -> Result<u64, String> {
    bytes
        .get(offset..offset.saturating_add(8))
        .and_then(|slice| slice.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or_else(|| format!("missing u64 at segment offset {offset}"))
}

fn run_orphan_omission_fault_probe() -> Result<(), String> {
    let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
    let mut engine = RealEngine::without_faults(directory.path().to_path_buf());
    engine.open()?;
    engine.ingest(&[DocMutation {
        doc_id: 1,
        revision: 1,
        timestamp: 10,
    }])?;
    engine.seal()?;
    engine.close()?;
    let source = first_segment_path(directory.path())?;
    let orphan = directory.path().join("segment-orphan.zseg");
    std::fs::copy(&source, &orphan).map_err(|error| error.to_string())?;
    let event = FaultEvent {
        id: "feature-storage-list-omission".to_owned(),
        op_index: 0,
        layer: fault_vfs::Layer::Content,
        site: fault_vfs::FaultSite::List,
        mode: fault_vfs::FaultMode::SilentDrop,
        nth_match: 1,
        expected_matches: None,
        deadline_budget_seconds: None,
        path_contains: None,
        fired: false,
        fire_count: 0,
        path: None,
    };
    let scheduled = Arc::new(fault_vfs::std_scheduled(fault_vfs::FaultSchedule::single(
        event,
    )));
    scheduled.set_operation(0);
    zeppelin_embed::manifest::io::open_manifest(
        scheduled.as_ref(),
        directory.path(),
        &FeatureDurableLog,
        &HashSet::new(),
    )
    .map_err(|error| error.to_string())?;
    if !scheduled.events().into_iter().any(|event| event.fired) {
        return Err("list omission did not reach manifest orphan cleanup".to_owned());
    }
    if !orphan.exists() {
        return Err("list omission unexpectedly exposed and deleted the orphan".to_owned());
    }
    zeppelin_embed::manifest::io::open_manifest(
        &StdVfs,
        directory.path(),
        &FeatureDurableLog,
        &HashSet::new(),
    )
    .map_err(|error| error.to_string())?;
    if orphan.exists() {
        return Err("clean retry retained an eligible orphan".to_owned());
    }
    Ok(())
}

struct FeatureDurableLog;

impl zeppelin_embed::manifest::io::DurableLog for FeatureDurableLog {
    fn durable_end(&self) -> u64 {
        u64::MAX
    }
}

fn first_segment_path(directory: &Path) -> Result<PathBuf, String> {
    std::fs::read_dir(directory)
        .map_err(|error| error.to_string())?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("segment-") && name.ends_with(".zseg"))
        })
        .ok_or_else(|| "feature probe found no sealed segment".to_owned())
}

fn run_publication_crash_fault_probe(boundary: program::CrashBoundary) -> Result<(), String> {
    let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
    let mut engine = RealEngine::without_faults(directory.path().to_path_buf());
    engine.open()?;
    engine.ingest(&[DocMutation {
        doc_id: 1,
        revision: 1,
        timestamp: 10,
    }])?;
    let recovery = engine.crash_at_boundary(
        DocMutation {
            doc_id: 2,
            revision: 1,
            timestamp: 11,
        },
        boundary,
        CampaignKind::Overall,
        0,
        Program::generate(0)
            .ops
            .iter()
            .position(|op| matches!(op, Op::Crash { .. }))
            .ok_or_else(|| "overall program has no crash operation".to_owned())?,
        FaultProfile::None,
    )?;
    let observed = engine.search(&program::query(3), 2, SearchKind::Scan, 0)?;
    let ids = observed
        .hits
        .iter()
        .map(|hit| hit.doc_id)
        .collect::<BTreeSet<_>>();
    if !ids.contains(&1) {
        return Err("publication crash lost the previously committed document".to_owned());
    }
    match recovery.disposition {
        CrashDisposition::Unacknowledged if !ids.contains(&2) => {}
        CrashDisposition::Visible if ids.contains(&2) => {}
        CrashDisposition::Gone => {
            return Err("publication crash reported an impossible purge state".to_owned());
        }
        disposition => {
            return Err(format!(
                "publication crash receipt {disposition:?} disagreed with reopened ids {ids:?}"
            ));
        }
    }
    engine.close()
}

fn run_kernel_parity_probe(coverage: &mut CoverageRegistry) -> Result<(), String> {
    let left_i8 = [-3_i8, -1, 0, 1, 2, 3, 7, -8];
    let right_i8 = [2_i8, -4, 9, 5, -2, 1, 3, -1];
    let expected_i8 = left_i8
        .iter()
        .zip(right_i8)
        .map(|(left, right)| i32::from(*left) * i32::from(right))
        .sum::<i32>();
    let left_bits = [0b1010_0101_u8, 0b1111_0000];
    let right_bits = [0b0011_1100_u8, 0b1100_0011];
    let expected_hamming = left_bits
        .iter()
        .zip(right_bits)
        .map(|(left, right)| (left ^ right).count_ones())
        .sum::<u32>();
    let left_f16 = [0x3c00_u16, 0x4000];
    let right_f16 = [0x4200_u16, 0x4400];
    let left_f32 = [1.0_f32, 2.0, -3.0, 4.0];
    let right_f32 = [5.0_f32, -2.0, 1.0, 0.5];
    let expected_f32 = left_f32
        .iter()
        .zip(right_f32)
        .map(|(left, right)| left * right)
        .sum::<f32>();
    for variant in zeppelin_embed::kernels::KernelVariant::available() {
        let backend = format!("{:?}", variant.arm()).to_ascii_lowercase();
        if variant.dot_i8(&left_i8, &right_i8) != expected_i8 {
            return Err(format!("{backend} i8 dot diverged from literal scalar sum"));
        }
        if variant.hamming_u1(&left_bits, &right_bits) != expected_hamming {
            return Err(format!(
                "{backend} Hamming diverged from literal xor/popcount"
            ));
        }
        if variant.dot_f16(&left_f16, &right_f16).to_bits() != 11.0_f32.to_bits() {
            return Err(format!("{backend} f16 dot diverged from literal value 11"));
        }
        if variant.dot_f32(&left_f32, &right_f32).to_bits() != expected_f32.to_bits() {
            return Err(format!(
                "{backend} f32 dot diverged from literal scalar sum"
            ));
        }
        coverage.hit(format!("kernel.backend.{backend}"));
    }
    Ok(())
}

fn run_fts_extras_probe(slot: u8) -> Result<(), String> {
    let analyzer = Analyzer::new(Profile::Code.config()).map_err(|error| error.to_string())?;
    let texts = ["alpha bravo charli", "alpha x bravo", "delta echo"];
    let mut segment = SegmentIndex::new();
    for text in texts {
        segment
            .push_document(&analyzer, &Document::with_text(text))
            .map_err(|error| error.to_string())?;
    }
    let slop = u32::from(slot % 2);
    let actual_phrase = phrase::search_segment(
        &segment,
        &PhraseQuery {
            terms: vec![b"alpha".to_vec(), b"bravo".to_vec()],
            slop,
            field: DEFAULT_FIELD,
        },
    )
    .map_err(|error| error.to_string())?;
    let expected_phrase = if slop == 0 { vec![0] } else { vec![0, 1] };
    if actual_phrase != expected_phrase {
        return Err(format!(
            "phrase oracle mismatch: expected={expected_phrase:?} actual={actual_phrase:?}"
        ));
    }

    let vocabulary = [b"alpha".as_slice(), b"alpine", b"bravo", b"charli"];
    let mut dictionary = TermDictionary::default();
    for term in vocabulary {
        dictionary
            .push(term, TermInfo::default())
            .map_err(|error| error.to_string())?;
    }
    let prefix_term = if slot.is_multiple_of(2) { b"al" } else { b"br" };
    let actual_prefix = prefix::search(&dictionary, prefix_term)
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|matched| matched.term)
        .collect::<Vec<_>>();
    let expected_prefix = vocabulary
        .iter()
        .filter(|term| term.starts_with(prefix_term))
        .map(|term| term.to_vec())
        .collect::<Vec<_>>();
    if actual_prefix != expected_prefix {
        return Err(format!(
            "prefix oracle mismatch: expected={expected_prefix:?} actual={actual_prefix:?}"
        ));
    }

    let fuzzy_query = b"alpga";
    let (actual_fuzzy, _) = fuzzy::search(&dictionary, fuzzy_query, 1);
    let actual_fuzzy = actual_fuzzy
        .into_iter()
        .map(|candidate| (candidate.term, candidate.distance))
        .collect::<Vec<_>>();
    let expected_fuzzy = vocabulary
        .iter()
        .filter_map(|term| {
            let distance = naive_edit_distance(fuzzy_query, term);
            (term.first() == fuzzy_query.first() && distance <= 1)
                .then(|| (term.to_vec(), distance as u32))
        })
        .collect::<Vec<_>>();
    if actual_fuzzy != expected_fuzzy {
        return Err(format!(
            "fuzzy oracle mismatch: expected={expected_fuzzy:?} actual={actual_fuzzy:?}"
        ));
    }

    let encoded = phonetic::encode(if slot.is_multiple_of(2) {
        "Smith"
    } else {
        "Schmidt"
    });
    let expected_code = if slot.is_multiple_of(2) { "SM0" } else { "XMT" };
    if encoded != expected_code {
        return Err(format!(
            "phonetic golden mismatch: expected={expected_code} actual={encoded}"
        ));
    }

    let snippet_text = "zero alpha beta omega";
    let terms = vec![b"alpha".to_vec(), b"beta".to_vec()];
    let built = snippet::best_window(&analyzer, snippet_text, &terms, 10, true)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "snippet probe returned no matching window".to_owned())?;
    if built.text(snippet_text) != Some("alpha beta") {
        return Err(format!(
            "snippet window mismatch: expected=\"alpha beta\" actual={:?}",
            built.text(snippet_text)
        ));
    }
    for highlight in &built.highlights {
        let start = usize::try_from(highlight.start).map_err(|_| "snippet start overflow")?;
        let end = usize::try_from(highlight.end).map_err(|_| "snippet end overflow")?;
        let surface = snippet_text
            .get(start..end)
            .ok_or_else(|| "snippet highlight escaped UTF-8 boundaries".to_owned())?;
        if !matches!(surface, "alpha" | "beta") {
            return Err(format!("snippet highlighted non-match {surface:?}"));
        }
    }
    Ok(())
}

fn naive_edit_distance(left: &[u8], right: &[u8]) -> usize {
    let mut previous = (0..=right.len()).collect::<Vec<_>>();
    for (left_index, left_byte) in left.iter().enumerate() {
        let mut current = Vec::with_capacity(right.len().saturating_add(1));
        current.push(left_index.saturating_add(1));
        for (right_index, right_byte) in right.iter().enumerate() {
            let deletion = previous[right_index.saturating_add(1)].saturating_add(1);
            let insertion = current[right_index].saturating_add(1);
            let substitution =
                previous[right_index].saturating_add(usize::from(left_byte != right_byte));
            current.push(deletion.min(insertion).min(substitution));
        }
        previous = current;
    }
    previous.last().copied().unwrap_or(left.len())
}

#[must_use]
pub fn runner_retries_faulted_operation(events: &[FaultEvent], op_index: usize) -> bool {
    if events.iter().any(|event| {
        event.op_index == op_index && event.fired && event.layer == fault_vfs::Layer::Content
    }) {
        return false;
    }
    events.iter().any(|event| {
        event.op_index == op_index
            && event.fired
            && event.layer == fault_vfs::Layer::Io
            && matches!(
                event.mode,
                fault_vfs::FaultMode::Eio
                    | fault_vfs::FaultMode::Eacces
                    | fault_vfs::FaultMode::Enospc
                    | fault_vfs::FaultMode::PostCommitError
                    | fault_vfs::FaultMode::Latency
            )
    })
}

#[must_use]
pub fn runner_records_operation_error(events: &[FaultEvent], op_index: usize) -> bool {
    runner_records_error(events, op_index, RunnerErrorSource::Operation)
}

#[must_use]
pub fn runner_records_error(
    events: &[FaultEvent],
    op_index: usize,
    source: RunnerErrorSource,
) -> bool {
    source == RunnerErrorSource::BusyHook
        || !events.iter().any(|event| {
            event.fired && event.layer == fault_vfs::Layer::Content && event.op_index == op_index
        })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunnerErrorSource {
    Operation,
    BusyHook,
}

fn clock_jump_requires_timeout(events: &[FaultEvent]) -> bool {
    events.iter().any(|event| {
        matches!(
            event.mode,
            fault_vfs::FaultMode::ClockJump { seconds }
                if event
                    .deadline_budget_seconds
                    .is_some_and(|budget| seconds >= budget)
        )
    })
}

fn persisted_content_fault_preceded(events: &[FaultEvent], op_index: usize) -> bool {
    events.iter().any(|event| {
        event.fired
            && event.op_index <= op_index
            && event.layer == fault_vfs::Layer::Content
            && matches!(
                event.site,
                fault_vfs::FaultSite::Append | fault_vfs::FaultSite::Write
            )
    })
}

fn graph_build_crash_preceded(
    events: &[FaultEvent],
    op_index: usize,
    since: Option<usize>,
) -> bool {
    events.iter().any(|event| {
        event.fired
            && event.layer == fault_vfs::Layer::Crash
            && event.op_index < op_index
            && since.is_none_or(|since| event.op_index > since)
            && event
                .path
                .as_ref()
                .is_some_and(|path| path.to_string_lossy().contains(".graph.checkpoint"))
    })
}

fn persisted_content_fault_before(events: &[FaultEvent], op_index: usize) -> bool {
    op_index
        .checked_sub(1)
        .is_some_and(|previous| persisted_content_fault_preceded(events, previous))
}

fn wal_rewrite_refusal(error: &str) -> bool {
    error.contains("purge WAL rewrite would drop acknowledged WAL sequence ")
}

fn recover_and_retry_faulted_operation(
    engine: &mut RealEngine,
    model: &mut Model,
    op: &Op,
) -> Result<Option<MutationAck>, String> {
    engine.reopen()?;
    match op {
        Op::Ingest {
            first_id,
            count,
            revision,
            timestamp,
        } => {
            let documents = (*first_id..first_id.saturating_add(*count))
                .map(|doc_id| DocMutation {
                    doc_id,
                    revision: *revision,
                    timestamp: *timestamp,
                })
                .collect::<Vec<_>>();
            let ack = engine.ingest(&documents)?;
            for document in documents {
                model.acknowledge(document.doc_id, document.revision, document.timestamp);
            }
            Ok(Some(ack))
        }
        Op::Upsert {
            doc_id,
            revision,
            timestamp,
        }
        | Op::Revise {
            doc_id,
            revision,
            timestamp,
        } => {
            let document = DocMutation {
                doc_id: *doc_id,
                revision: *revision,
                timestamp: *timestamp,
            };
            let ack = engine.ingest(&[document])?;
            model.acknowledge(*doc_id, *revision, *timestamp);
            Ok(Some(ack))
        }
        Op::Delete { doc_id } => {
            let ack = engine.delete(*doc_id)?;
            model.delete(*doc_id);
            Ok(Some(ack))
        }
        Op::Seal => engine.seal().map(|ack| {
            model.seal();
            Some(ack)
        }),
        Op::DropPartition { start, end } => {
            let ack = engine.drop_partition(*start, *end)?;
            model.drop_partition(*start, *end);
            Ok(Some(ack))
        }
        Op::Purge { doc_id } => {
            let ack = engine.purge(*doc_id)?;
            model.purge(*doc_id);
            Ok(Some(ack))
        }
        Op::Open | Op::Reopen => Ok(None),
        Op::Maintain { bytes } => engine.maintain(*bytes).map(Some),
        other => Err(format!(
            "no idempotent fault retry is defined for {}",
            other.kind()
        )),
    }
}

fn reconcile_simulated_crash(
    model: &mut Model,
    op: &Op,
    crash: &FaultEvent,
    wal_dirent_durable: bool,
) -> Result<(), String> {
    if crash.layer != fault_vfs::Layer::Crash || crash.mode != fault_vfs::FaultMode::Crash {
        return Err("simulated crash recovery received a non-Crash event".to_owned());
    }
    match op {
        Op::Ingest {
            first_id,
            count,
            revision,
            timestamp,
        } => match crash.site {
            fault_vfs::FaultSite::Append => {}
            fault_vfs::FaultSite::Sync => {
                if wal_dirent_durable && crash_path_is(crash, "wal.ze") {
                    for doc_id in *first_id..first_id.saturating_add(*count) {
                        model.acknowledge(doc_id, *revision, *timestamp);
                    }
                }
            }
            site => {
                return Err(format!("Crash/{site:?} is not a durable ingest boundary"));
            }
        },
        Op::Upsert {
            doc_id,
            revision,
            timestamp,
        }
        | Op::Revise {
            doc_id,
            revision,
            timestamp,
        } => match crash.site {
            fault_vfs::FaultSite::Append => {}
            fault_vfs::FaultSite::Sync => {
                if wal_dirent_durable && crash_path_is(crash, "wal.ze") {
                    model.acknowledge(*doc_id, *revision, *timestamp);
                }
            }
            site => {
                return Err(format!(
                    "Crash/{site:?} is not a durable {} boundary",
                    op.kind()
                ));
            }
        },
        Op::Delete { doc_id } => match crash.site {
            fault_vfs::FaultSite::Append => {}
            fault_vfs::FaultSite::Sync => {
                if wal_dirent_durable && crash_path_is(crash, "wal.ze") {
                    model.delete(*doc_id);
                }
            }
            site => {
                return Err(format!("Crash/{site:?} is not a durable delete boundary"));
            }
        },
        Op::Purge { .. } => match crash.site {
            // The scheduled first Append/Sync boundary is before the purge
            // intent rename and manifest omission. It cannot make the purge
            // logically durable merely because its temporary bytes synced.
            fault_vfs::FaultSite::Append | fault_vfs::FaultSite::Sync => {}
            site => {
                return Err(format!("Crash/{site:?} is not a durable purge boundary"));
            }
        },
        // A scheduled Seal crash fires before the manifest rename is made
        // directory-durable. The durable WAL independently preserves the
        // logical document set in the active segment after reopen.
        Op::Seal => match crash.site {
            fault_vfs::FaultSite::Write
            | fault_vfs::FaultSite::Sync
            | fault_vfs::FaultSite::Rename
            | fault_vfs::FaultSite::Delete => {}
            site => {
                return Err(format!("Crash/{site:?} is not a Seal boundary"));
            }
        },
        // DropPartition unlinks only after its manifest omission is committed
        // and published, so any reachable Delete crash retains that omission.
        Op::DropPartition { start, end } => {
            if crash.site != fault_vfs::FaultSite::Delete {
                return Err(format!(
                    "Crash/{:?} is not a DropPartition boundary",
                    crash.site
                ));
            }
            model.drop_partition(*start, *end);
        }
        // Maintenance changes derived accelerators, never logical documents.
        Op::Maintain { .. } => match crash.site {
            fault_vfs::FaultSite::Write
            | fault_vfs::FaultSite::Sync
            | fault_vfs::FaultSite::Rename
            | fault_vfs::FaultSite::Delete => {}
            site => {
                return Err(format!("Crash/{site:?} is not a Maintain boundary"));
            }
        },
        _ => {
            return Err(format!(
                "Crash event reached unsupported {} operation",
                op.kind()
            ));
        }
    }
    Ok(())
}

fn reconcile_recovered_locations(engine: &mut RealEngine, model: &mut Model) -> Result<(), String> {
    let snapshot = engine
        .store()?
        .snapshot()
        .map_err(|error| error.to_string())?;
    let mut sealed = BTreeSet::new();
    for segment in snapshot.segments() {
        for row in 0..segment.meta().row_count as usize {
            let Some(version) = segment
                .document_version(row)
                .map_err(|error| error.to_string())?
            else {
                continue;
            };
            let doc_id = u32::try_from(version.doc_id().get())
                .map_err(|_| "sealed document id exceeds adversarial vocabulary")?;
            sealed.insert((doc_id, version.revision().get()));
        }
    }
    for (doc_id, revision, timestamp) in model.live_documents() {
        if !sealed.contains(&(doc_id, revision)) {
            model.acknowledge(doc_id, revision, timestamp);
        }
    }
    Ok(())
}

fn crash_path_is(crash: &FaultEvent, expected: &str) -> bool {
    crash
        .path
        .as_deref()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        == Some(expected)
}

#[must_use]
pub fn simulated_crash_torn_middle_counterexample() -> Violation {
    let seed = 92_004;
    let mut model = Model::default();
    let operation = Op::Ingest {
        first_id: 5,
        count: 3,
        revision: 1,
        timestamp: 10,
    };
    let crash = FaultEvent {
        id: "torn-middle-crash".to_owned(),
        op_index: 1,
        layer: fault_vfs::Layer::Crash,
        site: fault_vfs::FaultSite::Sync,
        mode: fault_vfs::FaultMode::Crash,
        nth_match: 1,
        expected_matches: None,
        deadline_budget_seconds: None,
        path_contains: Some("wal.ze".to_owned()),
        fired: true,
        fire_count: 1,
        path: Some(PathBuf::from("wal.ze")),
    };
    reconcile_simulated_crash(&mut model, &operation, &crash, true)
        .expect("synced ingest crash boundary is supported");
    let expected = model.expected_scan(&program::query(3), model.len());
    let hits = expected
        .into_iter()
        .filter(|hit| hit.doc_id != 6)
        .map(|hit| Hit {
            doc_id: hit.doc_id,
            revision: hit.revision,
            score: hit.score,
        })
        .collect::<Vec<_>>();
    let observed = SearchObservation {
        diagnostics_requested_k: model.len(),
        diagnostics_returned: hits.len(),
        hits,
        generation: 1,
        epoch: Some(declared_identity()),
        graph_available: false,
        graph_segments: 0,
        graph_rescored: 0,
        graph_pruned: 0,
        diagnostics_approximate: false,
        diagnostics_exact_rescore: false,
        diagnostics_budget_exhausted: false,
        diagnostics_counters_match: true,
        diagnostics_plan_matches_execution: true,
        expected_exact_rescore: false,
        expected_budget_exhausted: false,
    };
    durability_prefix_violation(seed, FaultProfile::Crash, 1, &model, &observed)
        .expect("torn middle must lose an independently expected durable id")
}

struct HybridCheck {
    sealed_vector_documents: usize,
    lexical_documents: usize,
    store_lexical_searches: usize,
    store_hybrid_searches: usize,
    mismatch: Option<String>,
}

fn run_hybrid_search(
    engine: &mut dyn Engine,
    model: &Model,
    query_slot: u8,
    k: usize,
    seed: u64,
) -> Result<Option<HybridCheck>, String> {
    let sealed_vector_documents = model.sealed_document_count();
    let query_vector = program::query(query_slot);
    let observed = engine.search(&query_vector, model.len(), SearchKind::Auto, seed)?;
    if sealed_vector_documents == 0 || model.is_empty() {
        return Ok(None);
    }
    if !observed.graph_available {
        return Ok(None);
    }

    let mut expected_lexical = model
        .expected_lexical(query_slot, model.len())
        .into_iter()
        .map(|hit| LexicalHit {
            doc_id: hit.doc_id,
            revision: hit.revision,
            score: hit.score,
        })
        .collect::<Vec<_>>();
    let mut actual_lexical = engine.lexical_search(query_slot, model.len())?;
    let semantic_order = |left: &LexicalHit, right: &LexicalHit| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.doc_id.cmp(&right.doc_id))
            .then_with(|| left.revision.cmp(&right.revision))
    };
    expected_lexical.sort_by(semantic_order);
    actual_lexical.sort_by(semantic_order);
    let lexical_mismatch = (actual_lexical.len() != expected_lexical.len()
        || actual_lexical
            .iter()
            .zip(&expected_lexical)
            .any(|(actual, expected)| {
                actual.doc_id != expected.doc_id
                    || actual.revision != expected.revision
                    || !close_f64(actual.score, expected.score)
            }))
    .then(|| {
        format!("lexical result mismatch: model={expected_lexical:?} engine={actual_lexical:?}")
    });

    let actual = engine.hybrid_search(query_slot, k)?;
    let expected = model.expected_hybrid(&query_vector, query_slot, k);
    let hybrid_mismatch = (actual.hits.len() != expected.len()
        || actual.hits.iter().zip(&expected).any(|(actual, expected)| {
            actual.doc_id != expected.doc_id
                || !close_optional_f64(actual.vector_squared_l2, expected.vector_squared_l2)
                || !close_optional_f64(actual.lexical_bm25, expected.lexical_bm25)
                || !close_f64(actual.fused_score, expected.fused_score)
        }))
    .then(|| {
        format!(
            "hybrid exact result mismatch: model={expected:?} engine={:?}",
            actual.hits
        )
    });
    let mismatch = if let Some(detail) = lexical_mismatch.or(hybrid_mismatch) {
        Some(detail)
    } else if actual.report_epoch != Some(declared_identity()) {
        Some(format!(
            "hybrid report epoch {:?} did not name the declared store epoch",
            actual.report_epoch
        ))
    } else if actual.generation != observed.generation {
        Some(format!(
            "hybrid generation {} did not match vector generation {}",
            actual.generation, observed.generation
        ))
    } else {
        None
    };
    Ok(Some(HybridCheck {
        sealed_vector_documents,
        lexical_documents: model.lexical_documents().len(),
        store_lexical_searches: 1,
        store_hybrid_searches: 1,
        mismatch,
    }))
}

fn close_f64(left: f64, right: f64) -> bool {
    let scale = left.abs().max(right.abs()).max(1.0);
    (left - right).abs() <= f64::EPSILON * 16.0 * scale
}

fn close_optional_f64(left: Option<f64>, right: Option<f64>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => close_f64(left, right),
        (None, None) => true,
        _ => false,
    }
}

#[allow(clippy::too_many_arguments)]
fn check_filtered_search(
    seed: u64,
    profile: FaultProfile,
    op_index: usize,
    model: &Model,
    query: &[f32],
    k: usize,
    maximum_timestamp: i64,
    observed: &SearchObservation,
) -> Vec<Violation> {
    let mut violations = Vec::new();
    let available = model
        .expected_filtered(query, model.len(), maximum_timestamp)
        .len();
    if let Some(violation) = diagnostics_violation(seed, profile, op_index, k, available, observed)
    {
        violations.push(violation);
    }
    if let Some(hit) = observed.hits.iter().find(|hit| {
        model
            .timestamp(hit.doc_id)
            .is_none_or(|timestamp| timestamp > maximum_timestamp)
    }) {
        violations.push(violation(
            Invariant::I5,
            seed,
            profile,
            op_index,
            format!(
                "filtered query max_ts={maximum_timestamp} returned doc {} at timestamp {:?}",
                hit.doc_id,
                model.timestamp(hit.doc_id)
            ),
        ));
    }
    let expected = if observed.graph_available {
        model.expected_filtered(query, k, maximum_timestamp)
    } else {
        model.expected_filtered_scan(query, k, maximum_timestamp)
    };
    if let Some(detail) = exact_mismatch(&expected, &observed.hits) {
        violations.push(violation(Invariant::I3, seed, profile, op_index, detail));
    }
    violations
}

fn full_scan(
    engine: &mut dyn Engine,
    model: &Model,
    seed: u64,
) -> Result<SearchObservation, String> {
    engine.search(&program::query(3), model.len(), SearchKind::Scan, seed)
}

pub(crate) fn first_durable_ack_survives_wal_create_crash(
    seed: u64,
    profile: FaultProfile,
) -> Result<Option<Violation>, String> {
    let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
    let mut engine = RealEngine::without_faults(directory.path().to_path_buf());
    let document = DocMutation {
        doc_id: 1,
        revision: 1,
        timestamp: 1,
    };
    let mut model = Model::default();
    engine.open()?;
    engine.reopen()?;
    let ack = engine.ingest(&[document])?;
    if !ack.changed {
        return Err("first durable ingest returned an unchanged acknowledgement".to_owned());
    }
    model.acknowledge(document.doc_id, document.revision, document.timestamp);

    engine
        .vfs
        .simulate_crash()
        .map_err(|error| error.to_string())?;
    engine.recover_from_simulated_crash()?;
    let observed = full_scan(&mut engine, &model, seed)?;
    let violation = durability_prefix_violation(seed, profile, 0, &model, &observed);
    engine.close()?;
    Ok(violation)
}

fn warm_cancel_query(
    engine: &mut dyn Engine,
    model: &Model,
    operation: &Op,
    seed: u64,
) -> Result<(), String> {
    match operation {
        Op::Search { query, k, kind } => {
            let requested = if *k == usize::MAX { model.len() } else { *k };
            let _ = engine.search(&program::query(*query), requested, *kind, seed)?;
        }
        Op::FilteredSearch {
            query,
            k,
            maximum_timestamp,
        } => {
            let requested = if *k == usize::MAX { model.len() } else { *k };
            let _ =
                engine.filtered_search(&program::query(*query), requested, *maximum_timestamp)?;
        }
        Op::PredicateSearch {
            query,
            k,
            predicate,
        } => {
            let requested = if *k == usize::MAX { model.len() } else { *k };
            let _ = engine.predicate_search(&program::query(*query), requested, *predicate)?;
        }
        Op::HybridSearch { query, k } => {
            let requested = if *k == usize::MAX { model.len() } else { *k };
            let _ = run_hybrid_search(engine, model, *query, requested, seed)?;
        }
        Op::DeadlineProbe { query } => {
            let _ = engine.search(&program::query(*query), 1, SearchKind::Scan, seed)?;
        }
        _ => {}
    }
    Ok(())
}

fn durability_prefix_violation(
    seed: u64,
    profile: FaultProfile,
    op_index: usize,
    model: &Model,
    observed: &SearchObservation,
) -> Option<Violation> {
    let returned = observed
        .hits
        .iter()
        .map(|hit| hit.doc_id)
        .collect::<BTreeSet<_>>();
    let missing = model
        .live_ids()
        .difference(&returned)
        .copied()
        .collect::<Vec<_>>();
    (!missing.is_empty()).then(|| {
        violation(
            Invariant::I4,
            seed,
            profile,
            op_index,
            format!("durable crash recovery lost acknowledged ids {missing:?}"),
        )
    })
}

#[allow(clippy::too_many_arguments)]
fn check_search(
    seed: u64,
    profile: FaultProfile,
    op_index: usize,
    model: &Model,
    query: &[f32],
    k: usize,
    kind: SearchKind,
    observed: &SearchObservation,
    content_fault_fired: bool,
) -> Vec<Violation> {
    let mut violations = Vec::new();
    if let Some(violation) =
        diagnostics_violation(seed, profile, op_index, k, model.len(), observed)
    {
        violations.push(violation);
    }
    if let Some(violation) = epoch_identity_violation(
        seed,
        profile,
        op_index,
        identity_for(model.published_epoch()),
        observed,
    ) {
        violations.push(violation);
    }
    let forbidden = model.forbidden_ids();
    if let Some(hit) = observed
        .hits
        .iter()
        .find(|hit| forbidden.contains(&hit.doc_id))
    {
        violations.push(violation(
            Invariant::I2,
            seed,
            profile,
            op_index,
            format!(
                "returned forbidden doc {} revision {}",
                hit.doc_id, hit.revision
            ),
        ));
    }
    if let Some(hit) = observed.hits.iter().find(|hit| {
        model
            .revision(hit.doc_id)
            .is_some_and(|revision| hit.revision != revision)
    }) {
        violations.push(violation(
            Invariant::I11,
            seed,
            profile,
            op_index,
            format!(
                "returned doc {} revision {}, highest acknowledged is {}",
                hit.doc_id,
                hit.revision,
                model.revision(hit.doc_id).unwrap_or_default()
            ),
        ));
    }
    if k >= model.len() {
        let returned = observed
            .hits
            .iter()
            .map(|hit| hit.doc_id)
            .collect::<BTreeSet<_>>();
        let missing = model
            .live_ids()
            .difference(&returned)
            .copied()
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            violations.push(violation(
                Invariant::I1,
                seed,
                profile,
                op_index,
                format!("acknowledged docs missing from full query: {missing:?}"),
            ));
        }
    }
    let exact = kind == SearchKind::Scan || (kind == SearchKind::Auto && !observed.graph_available);
    let expected = if exact {
        model.expected_scan(query, k)
    } else {
        model.expected(query, k)
    };
    if exact {
        if let Some(detail) = exact_mismatch(&expected, &observed.hits) {
            if model.epoch_b_prepared() {
                violations.push(violation(
                    Invariant::I14,
                    seed,
                    profile,
                    op_index,
                    format!("published epoch results are incomplete or mixed: {detail}"),
                ));
            }
            violations.push(violation(
                Invariant::I3,
                seed,
                profile,
                op_index,
                detail.clone(),
            ));
            if content_fault_fired {
                violations.push(violation(
                    Invariant::I7,
                    seed,
                    profile,
                    op_index,
                    format!("content fault was consumed into a successful wrong result: {detail}"),
                ));
            }
        }
    } else if !expected.is_empty() {
        let expected_ids = expected
            .iter()
            .map(|hit| hit.doc_id)
            .collect::<BTreeSet<_>>();
        let retained = observed
            .hits
            .iter()
            .filter(|hit| expected_ids.contains(&hit.doc_id))
            .count();
        let recall = retained as f64 / expected.len() as f64;
        if recall < 0.80 {
            violations.push(violation(
                Invariant::I3,
                seed,
                profile,
                op_index,
                format!(
                    "approximate graph recall {:.3} below 0.800 ({retained}/{})",
                    recall,
                    expected.len()
                ),
            ));
        }
        let graph_work_missing =
            observed.graph_available && observed.graph_segments == 0 && observed.graph_pruned == 0;
        let graph_rescore_missing = observed.graph_segments != 0 && observed.graph_rescored == 0;
        if graph_work_missing || graph_rescore_missing {
            violations.push(violation(
                Invariant::I7,
                seed,
                profile,
                op_index,
                format!(
                    "graph request did not traverse/rescore a graph: traversed={} rescored={} pruned={} generation={}",
                    observed.graph_segments,
                    observed.graph_rescored,
                    observed.graph_pruned,
                    observed.generation
                ),
            ));
        }
    }
    violations
}

fn diagnostics_violation(
    seed: u64,
    profile: FaultProfile,
    op_index: usize,
    requested_k: usize,
    available_matches: usize,
    observed: &SearchObservation,
) -> Option<Violation> {
    let expected_approximate = observed.graph_segments != 0;
    let silent_shortfall = observed.hits.len() < requested_k
        && available_matches >= requested_k
        && !observed.diagnostics_budget_exhausted;
    let mut lies = Vec::new();
    if observed.diagnostics_requested_k != requested_k {
        lies.push(format!(
            "requested_k={} expected {requested_k}",
            observed.diagnostics_requested_k
        ));
    }
    if observed.diagnostics_returned != observed.hits.len() {
        lies.push(format!(
            "returned={} actual {}",
            observed.diagnostics_returned,
            observed.hits.len()
        ));
    }
    if observed.diagnostics_approximate != expected_approximate {
        lies.push(format!(
            "approximate={} expected {expected_approximate}",
            observed.diagnostics_approximate
        ));
    }
    if observed.diagnostics_exact_rescore != observed.expected_exact_rescore {
        lies.push(format!(
            "exact_rescore={} expected {}",
            observed.diagnostics_exact_rescore, observed.expected_exact_rescore
        ));
    }
    if observed.diagnostics_budget_exhausted != observed.expected_budget_exhausted {
        lies.push(format!(
            "budget_exhausted={} expected {}",
            observed.diagnostics_budget_exhausted, observed.expected_budget_exhausted
        ));
    }
    if !observed.diagnostics_counters_match {
        lies.push("composed counters differ from executor counters".to_owned());
    }
    if !observed.diagnostics_plan_matches_execution {
        lies.push("reported plan differs from executor plan".to_owned());
    }
    if silent_shortfall {
        lies.push(format!(
            "silent shortfall returned {} of {requested_k} with {available_matches} matches",
            observed.hits.len()
        ));
    }
    (!lies.is_empty()).then(|| violation(Invariant::I13, seed, profile, op_index, lies.join("; ")))
}

fn epoch_identity_violation(
    seed: u64,
    profile: FaultProfile,
    op_index: usize,
    expected: EpochIdentity,
    observed: &SearchObservation,
) -> Option<Violation> {
    (observed.epoch != Some(expected)).then(|| {
        violation(
            Invariant::I12,
            seed,
            profile,
            op_index,
            format!(
                "search response epoch {:?} did not name declared store epoch {expected:?}",
                observed.epoch
            ),
        )
    })
}

fn alias_visibility_violation(
    seed: u64,
    profile: FaultProfile,
    op_index: usize,
    model: &Model,
    visible: &[EpochId],
) -> Option<Violation> {
    let expected = identity_for(model.published_epoch()).embedding;
    let empty_with_live_documents = !model.is_empty() && visible.is_empty();
    let mixed = visible.iter().any(|epoch| *epoch != expected);
    (empty_with_live_documents || mixed).then(|| {
        violation(
            Invariant::I14,
            seed,
            profile,
            op_index,
            format!(
                "published alias expected only {expected:?}, visible segment epochs were {visible:?}"
            ),
        )
    })
}

fn exact_mismatch(expected: &[ExpectedHit], observed: &[Hit]) -> Option<String> {
    if expected.len() != observed.len() {
        return Some(format!(
            "exact result length mismatch: model={} engine={}",
            expected.len(),
            observed.len()
        ));
    }
    expected
        .iter()
        .zip(observed)
        .enumerate()
        .find_map(|(rank, (expected, observed))| {
            (expected.doc_id != observed.doc_id
                || expected.revision != observed.revision
                || expected.score.to_bits() != observed.score.to_bits())
            .then(|| {
                format!(
                    "exact rank {rank} mismatch: model=({},r{},{}:{:08x}) engine=({},r{},{}:{:08x})",
                    expected.doc_id,
                    expected.revision,
                    expected.score,
                    expected.score.to_bits(),
                    observed.doc_id,
                    observed.revision,
                    observed.score,
                    observed.score.to_bits()
                )
            })
        })
}

fn generation_violation(
    seed: u64,
    profile: FaultProfile,
    op_index: usize,
    previous: u64,
    ack: MutationAck,
) -> Option<Violation> {
    (ack.generation <= previous).then(|| {
        violation(
            Invariant::I9,
            seed,
            profile,
            op_index,
            format!(
                "mutation acknowledged generation {}, previous acknowledged generation was {previous}",
                ack.generation
            ),
        )
    })
}

fn stats_violation(
    seed: u64,
    profile: FaultProfile,
    op_index: usize,
    stats: StatsObservation,
) -> Option<Violation> {
    let contradiction = stats.active_segment_bytes > stats.resident_owned_bytes
        || stats.wal_bytes > stats.resident_owned_bytes
        || stats.cache_bytes > stats.resident_owned_bytes
        || stats.temporary_bytes > stats.resident_owned_bytes
        || stats.query_pool_bytes > stats.resident_owned_bytes
        || stats.mapped_bytes != stats.segment_bytes
        || stats.tombstone_bytes > stats.active_segment_bytes
        || stats.active_queries != 0;
    contradiction.then(|| {
        violation(
            Invariant::I6,
            seed,
            profile,
            op_index,
            format!("stats conservation contradiction: {stats:?}"),
        )
    })
}

fn lifecycle_stats_violation(
    seed: u64,
    profile: FaultProfile,
    op_index: usize,
    stats: StatsObservation,
) -> Option<Violation> {
    (stats.open_files != 2 || stats.active_queries != 0 || stats.active_snapshot_leases != 0)
        .then(|| {
            violation(
                Invariant::I8,
                seed,
                profile,
                op_index,
                format!(
                    "reopened lifecycle counters are not quiescent: open_files={} active_queries={} snapshot_leases={}",
                    stats.open_files, stats.active_queries, stats.active_snapshot_leases
                ),
            )
        })
}

fn purge_proof_violation(
    seed: u64,
    profile: FaultProfile,
    op_index: usize,
    directory: &Path,
    doc_id: u32,
) -> Option<Violation> {
    let needle = program::sentinel(doc_id);
    let mut stack = vec![directory.to_path_buf()];
    while let Some(path) = stack.pop() {
        let Ok(metadata) = std::fs::metadata(&path) else {
            continue;
        };
        if metadata.is_dir() {
            let Ok(entries) = std::fs::read_dir(&path) else {
                continue;
            };
            stack.extend(entries.filter_map(Result::ok).map(|entry| entry.path()));
        } else if let Ok(bytes) = std::fs::read(&path)
            && contains_bytes(&bytes, &needle)
        {
            return Some(violation(
                Invariant::I10,
                seed,
                profile,
                op_index,
                format!("purged sentinel remains in {}", path.display()),
            ));
        }
    }
    None
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn violation(
    invariant: Invariant,
    seed: u64,
    profile: FaultProfile,
    op_index: usize,
    detail: String,
) -> Violation {
    Violation {
        invariant,
        seed,
        profile,
        op_index,
        detail,
    }
}

#[must_use]
pub fn reproduction(seed: u64, profile: FaultProfile) -> String {
    reproduction_for(CampaignKind::Overall, seed, Some(profile))
}

#[must_use]
pub fn reproduction_for(
    campaign: CampaignKind,
    seed: u64,
    profile_override: Option<FaultProfile>,
) -> String {
    let profile = profile_override
        .map(|profile| format!("ZE_ADV_PROFILE={} ", profile.key()))
        .unwrap_or_default();
    if campaign == CampaignKind::Overall {
        return format!(
            "{profile}ZE_ADV_SEED={seed} cargo test -p zeppelin-embed-workspace-tests --test adversarial_tests run -- --ignored --exact --nocapture"
        );
    }
    format!(
        "{profile}ZE_ADV_CAMPAIGN={} ZE_ADV_SEED={seed} cargo test -p zeppelin-embed-workspace-tests --test adversarial_tests run -- --ignored --exact --nocapture",
        campaign.key()
    )
}

#[must_use]
pub fn reproduction_for_profile_override(
    campaign: CampaignKind,
    seed: u64,
    profile: FaultProfile,
    profile_overridden: bool,
) -> String {
    let profile_override = if profile_overridden {
        Some(profile)
    } else {
        None
    };
    reproduction_for(campaign, seed, profile_override)
}

fn json_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

pub fn planted_counterexample(invariant: Invariant) -> Violation {
    let seed = 91_000 + u64::from(invariant.number());
    let query = program::query(0);
    let observation = |hits: Vec<Hit>, graph_available, graph_segments, graph_rescored| {
        let returned = hits.len();
        let exact_rescore = returned == 0 || graph_segments != 0;
        SearchObservation {
            hits,
            generation: 1,
            epoch: Some(declared_identity()),
            graph_available,
            graph_segments,
            graph_rescored,
            graph_pruned: 0,
            diagnostics_requested_k: returned,
            diagnostics_returned: returned,
            diagnostics_approximate: graph_segments != 0,
            diagnostics_exact_rescore: exact_rescore,
            diagnostics_budget_exhausted: false,
            diagnostics_counters_match: true,
            diagnostics_plan_matches_execution: true,
            expected_exact_rescore: exact_rescore,
            expected_budget_exhausted: false,
        }
    };
    match invariant {
        Invariant::I5 => {
            let mut model = Model::default();
            model.acknowledge(1, 1, 10);
            let expected = model.expected_filtered(&query, 1, 10);
            let hit = expected.first().copied().expect("one filtered hit");
            let observed = observation(
                vec![Hit {
                    doc_id: hit.doc_id,
                    revision: hit.revision,
                    score: hit.score,
                }],
                false,
                0,
                0,
            );
            check_filtered_search(seed, FaultProfile::None, 1, &model, &query, 1, 9, &observed)
                .into_iter()
                .find(|violation| violation.invariant == Invariant::I5)
                .expect("filtered-out row must trip I5")
        }
        Invariant::I1 | Invariant::I3 | Invariant::I4 | Invariant::I7 | Invariant::I11 => {
            let mut model = Model::default();
            model.acknowledge(1, 2, 10);
            let expected = model.expected_scan(&query, 1);
            let expected_hit = expected.first().copied().expect("one expected hit");
            let observed = match invariant {
                Invariant::I1 | Invariant::I4 => observation(Vec::new(), false, 0, 0),
                Invariant::I3 => observation(
                    vec![Hit {
                        doc_id: 2,
                        revision: expected_hit.revision,
                        score: expected_hit.score,
                    }],
                    false,
                    0,
                    0,
                ),
                Invariant::I7 => {
                    let exact = model.expected(&query, 1);
                    let exact_hit = exact.first().copied().expect("one exact hit");
                    observation(
                        vec![Hit {
                            doc_id: exact_hit.doc_id,
                            revision: exact_hit.revision,
                            score: exact_hit.score,
                        }],
                        true,
                        0,
                        0,
                    )
                }
                Invariant::I11 => observation(
                    vec![Hit {
                        doc_id: expected_hit.doc_id,
                        revision: 1,
                        score: expected_hit.score,
                    }],
                    false,
                    0,
                    0,
                ),
                _ => unreachable!("grouped invariant is exhaustive"),
            };
            if invariant == Invariant::I4 {
                durability_prefix_violation(seed, FaultProfile::Crash, 1, &model, &observed)
                    .expect("missing durable id must trip I4")
            } else {
                let kind = if invariant == Invariant::I7 {
                    SearchKind::Graph
                } else {
                    SearchKind::Scan
                };
                check_search(
                    seed,
                    FaultProfile::None,
                    1,
                    &model,
                    &query,
                    1,
                    kind,
                    &observed,
                    false,
                )
                .into_iter()
                .find(|violation| violation.invariant == invariant)
                .unwrap_or_else(|| panic!("planted counterexample did not trip {invariant:?}"))
            }
        }
        Invariant::I2 => {
            let mut model = Model::default();
            model.acknowledge(1, 1, 10);
            model.delete(1);
            let observed = observation(
                vec![Hit {
                    doc_id: 1,
                    revision: 1,
                    score: 0.0,
                }],
                false,
                0,
                0,
            );
            check_search(
                seed,
                FaultProfile::None,
                1,
                &model,
                &query,
                0,
                SearchKind::Scan,
                &observed,
                false,
            )
            .into_iter()
            .find(|violation| violation.invariant == invariant)
            .expect("forbidden returned id must trip I2")
        }
        Invariant::I6 => {
            let stats = StatsObservation {
                resident_owned_bytes: 1,
                active_segment_bytes: 2,
                wal_bytes: 0,
                cache_bytes: 0,
                temporary_bytes: 0,
                query_pool_bytes: 0,
                mapped_bytes: 0,
                segment_bytes: 0,
                tombstone_bytes: 0,
                open_files: 2,
                active_queries: 0,
                active_snapshot_leases: 0,
            };
            stats_violation(seed, FaultProfile::None, 1, stats)
                .expect("contradictory stats must trip I6")
        }
        Invariant::I8 => {
            let stats = StatsObservation {
                resident_owned_bytes: 0,
                active_segment_bytes: 0,
                wal_bytes: 0,
                cache_bytes: 0,
                temporary_bytes: 0,
                query_pool_bytes: 0,
                mapped_bytes: 0,
                segment_bytes: 0,
                tombstone_bytes: 0,
                open_files: 3,
                active_queries: 0,
                active_snapshot_leases: 0,
            };
            lifecycle_stats_violation(seed, FaultProfile::None, 1, stats)
                .expect("leaked descriptor must trip I8")
        }
        Invariant::I9 => generation_violation(
            seed,
            FaultProfile::None,
            1,
            7,
            MutationAck {
                generation: 7,
                changed: true,
            },
        )
        .expect("stale generation must trip I9"),
        Invariant::I10 => {
            let directory = tempfile::tempdir().expect("I10 counterexample directory");
            std::fs::write(directory.path().join("wal.ze"), program::sentinel(1))
                .expect("I10 counterexample bytes");
            purge_proof_violation(seed, FaultProfile::None, 1, directory.path(), 1)
                .expect("persisted purge sentinel must trip I10")
        }
        Invariant::I12 => {
            let observed = SearchObservation {
                hits: Vec::new(),
                generation: 1,
                epoch: None,
                graph_available: false,
                graph_segments: 0,
                graph_rescored: 0,
                graph_pruned: 0,
                diagnostics_requested_k: 0,
                diagnostics_returned: 0,
                diagnostics_approximate: false,
                diagnostics_exact_rescore: true,
                diagnostics_budget_exhausted: false,
                diagnostics_counters_match: true,
                diagnostics_plan_matches_execution: true,
                expected_exact_rescore: true,
                expected_budget_exhausted: false,
            };
            epoch_identity_violation(seed, FaultProfile::None, 1, declared_identity(), &observed)
                .expect("missing response epoch must trip I12")
        }
        Invariant::I13 => {
            let observed = SearchObservation {
                diagnostics_returned: 1,
                ..observation(Vec::new(), false, 0, 0)
            };
            diagnostics_violation(seed, FaultProfile::None, 1, 0, 0, &observed)
                .expect("lying returned count must trip I13")
        }
        Invariant::I14 => {
            let mut model = Model::default();
            model.acknowledge(1, 1, 10);
            model.prepare_epoch_b();
            assert!(model.switch_epoch(ModelEpoch::B));
            alias_visibility_violation(
                seed,
                FaultProfile::None,
                1,
                &model,
                &[declared_identity().embedding],
            )
            .expect("mixed published epoch segments must trip I14")
        }
        Invariant::I54 => violation(
            Invariant::I54,
            seed,
            FaultProfile::Clock,
            1,
            "deadline was evaluated against a different monotonic clock".to_owned(),
        ),
        Invariant::I55 => violation(
            Invariant::I55,
            seed,
            FaultProfile::Full,
            1,
            "cancelled query returned partial results".to_owned(),
        ),
        Invariant::Feature(_) => {
            panic!("feature invariant plants live in the independent family oracle")
        }
    }
}

pub fn crash_child_from_env() -> Result<(), String> {
    let directory = PathBuf::from(
        std::env::var("ZE_ADV_CRASH_CHILD_PATH")
            .map_err(|_| "ZE_ADV_CRASH_CHILD_PATH is unset".to_owned())?,
    );
    let marker = PathBuf::from(
        std::env::var("ZE_ADV_CRASH_CHILD_MARKER")
            .map_err(|_| "ZE_ADV_CRASH_CHILD_MARKER is unset".to_owned())?,
    );
    let parse = |name: &str| -> Result<u64, String> {
        std::env::var(name)
            .map_err(|_| format!("{name} is unset"))?
            .parse::<u64>()
            .map_err(|error| format!("parse {name}: {error}"))
    };
    let doc_id = u32::try_from(parse("ZE_ADV_CRASH_CHILD_DOC")?)
        .map_err(|_| "crash child doc id exceeds u32".to_owned())?;
    let revision = parse("ZE_ADV_CRASH_CHILD_REV")?;
    let timestamp = parse("ZE_ADV_CRASH_CHILD_TS")? as i64;
    let seed = parse("ZE_ADV_SEED")?;
    let op_index = usize::try_from(parse("ZE_ADV_CRASH_CHILD_OP")?)
        .map_err(|_| "crash child op index exceeds usize".to_owned())?;
    let campaign = CampaignKind::from_key(
        &std::env::var("ZE_ADV_CAMPAIGN").map_err(|_| "ZE_ADV_CAMPAIGN is unset".to_owned())?,
    )?;
    let profile = match std::env::var("ZE_ADV_CRASH_CHILD_PROFILE")
        .or_else(|_| std::env::var("ZE_ADV_PROFILE"))
    {
        Ok(value) => FaultProfile::from_key(&value)?,
        Err(_) => profile_for_seed(seed),
    };
    let boundary = program::CrashBoundary::from_key(
        &std::env::var("ZE_ADV_CRASH_BOUNDARY")
            .map_err(|_| "ZE_ADV_CRASH_BOUNDARY is unset".to_owned())?,
    )?;
    let program = Program::generate_for(campaign, seed);
    let schedule = fault_vfs::plan_schedule(seed, environment_for_profile(profile, seed), &program);
    let mut scheduled = fault_vfs::ScheduledVfs::new(StdVfs, schedule);
    scheduled
        .set_fault_log(directory.join("faults.jsonl"))
        .map_err(|error| format!("write child fault plan: {error}"))?;
    let crash_vfs = Arc::new(fault_vfs::ProcessCrashVfs::new(scheduled.clone(), boundary));
    let store = Store::open_with_test_dependencies(
        &directory,
        RealEngine::options(ModelEpoch::A),
        StoreTestDependencies::new(crash_vfs.clone(), Arc::new(ManualMonotonicClock::new())),
    )
    .map_err(|error| error.to_string())?;
    scheduled.set_operation(op_index);
    let document = IngestDocument::new(
        DocumentVersion::new(DocId::new(u128::from(doc_id)), Revision::new(revision)),
        program::vector(doc_id, revision).to_vec(),
    )
    .with_timestamp(timestamp)
    .with_metadata(program::sentinel(doc_id))
    .with_text(program::lexical_text(doc_id, revision))
    .with_columns(adversarial_columns(doc_id));
    if boundary == program::CrashBoundary::MidWalGroup {
        crash_vfs.arm();
        let _ = store
            .ingest(IngestBatch::new(vec![document]).with_epoch(declared_identity()))
            .map_err(|error| error.to_string())?;
        return Err("mid-WAL crash boundary did not abort".to_owned());
    }

    let ack = store
        .ingest(IngestBatch::new(vec![document]).with_epoch(declared_identity()))
        .map_err(|error| error.to_string())?;
    std::fs::write(&marker, format!("{} visible", ack.generation()))
        .map_err(|error| format!("write crash acknowledgement: {error}"))?;

    if boundary == program::CrashBoundary::MidPurge {
        let deletion = store
            .delete(DeleteBatch::new(vec![DocId::new(u128::from(doc_id))]))
            .map_err(|error| error.to_string())?;
        let token = store
            .purge(&[DocId::new(u128::from(doc_id))])
            .map_err(|error| error.to_string())?;
        std::fs::write(&marker, format!("{} gone", deletion.generation()))
            .map_err(|error| format!("write crash purge acknowledgement: {error}"))?;
        crash_vfs.arm();
        let _ = store
            .await_physical_purge(token)
            .map_err(|error| error.to_string())?;
        return Err("mid-purge crash boundary did not abort".to_owned());
    }

    crash_vfs.arm();
    let _ = store
        .seal_with_cancel(&CancelToken::new())
        .map_err(|error| error.to_string())?;
    Err(format!("{} crash boundary did not abort", boundary.key()))
}

#[allow(dead_code)]
fn _keep_tempdir_type_visible(_: &TempDir) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vector_family_no_longer_round_trips_a_dead_clock_site() {
        let error = vector_generic_fault_schedule(&FaultEvent {
            id: "clock-adapter-refusal".to_owned(),
            op_index: 1,
            layer: fault_vfs::Layer::Clock,
            site: fault_vfs::FaultSite::Clock,
            mode: fault_vfs::FaultMode::ClockJump { seconds: 1 },
            nth_match: 1,
            expected_matches: None,
            deadline_budget_seconds: Some(1),
            path_contains: None,
            fired: false,
            fire_count: 0,
            path: None,
        })
        .expect_err("vector VFS adapter accepted a clock event");
        assert!(
            error.contains("cannot be routed through the vector VFS adapter"),
            "{error}"
        );
    }

    #[test]
    fn crash_after_coverage_counts_each_content_event_once() {
        let event = |id: &str, op_index, layer, mode| FaultEvent {
            id: id.to_owned(),
            op_index,
            layer,
            site: fault_vfs::FaultSite::Write,
            mode,
            nth_match: 1,
            expected_matches: None,
            deadline_budget_seconds: None,
            path_contains: None,
            fired: true,
            fire_count: 1,
            path: None,
        };
        let faults = [
            event(
                "content",
                1,
                fault_vfs::Layer::Content,
                fault_vfs::FaultMode::TornWrite,
            ),
            event(
                "first-crash",
                2,
                fault_vfs::Layer::Crash,
                fault_vfs::FaultMode::Crash,
            ),
            event(
                "second-crash",
                3,
                fault_vfs::Layer::Crash,
                fault_vfs::FaultMode::Crash,
            ),
        ];

        assert_eq!(
            crash_after_coverage_counts(&faults).get("torn_write"),
            Some(&1)
        );
    }

    #[test]
    fn deadline_probe_refuses_an_already_expired_deadline() {
        let directory = tempfile::tempdir().expect("deadline probe directory");
        let mut engine = RealEngine::without_faults(directory.path().to_path_buf());
        engine.open().expect("open deadline probe Store");
        engine
            .ingest(&[DocMutation {
                doc_id: 1,
                revision: 1,
                timestamp: 10,
            }])
            .expect("ingest deadline probe fixture");
        engine
            .arm_deadline_probe()
            .expect("arm expired deadline probe");
        let error = engine
            .search(&program::query(0), 1, SearchKind::Scan, 0)
            .expect_err("already-expired deadline was admitted");
        assert!(error.contains("deadline expired"), "{error}");
    }

    #[test]
    fn deadline_probe_is_independent_of_episode_wall_time() {
        let artifacts = tempfile::tempdir().expect("aged deadline probe artifacts");
        let aged_origin = std::time::Instant::now()
            .checked_sub(Duration::from_secs(180))
            .expect("180 seconds fits in the monotonic clock");
        let outcome = run_program_with_clock(
            0,
            FaultProfile::Clock,
            artifacts.path(),
            Arc::new(ManualMonotonicClock::starting_at(aged_origin)),
        )
        .expect("aged deadline probe run");

        assert!(
            outcome.violations.is_empty(),
            "an aged episode must still observe its injected deadline timeout: {:?}",
            outcome.violations
        );
    }
}
