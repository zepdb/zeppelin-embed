use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::{Read as _, Write as _};
use std::os::fd::{BorrowedFd, OwnedFd};
#[cfg(target_os = "macos")]
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
    vector_ceiling: Option<f64>,
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
            vector_ceiling: outcome.vector_ceiling,
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
            vector_ceiling: None,
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
            vector_ceiling: None,
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
    super::property_graph::probe(seed, &mut coverage)?;
    super::graph_contents::probe(seed, &mut coverage)?;
    super::property_graph_storage::probe(seed, &mut coverage)?;
    super::graph_catalog::probe(seed, &mut coverage)?;
    super::graph_key_lifecycle::probe(seed, &mut coverage)?;
    super::graph_query::probe(seed, &mut coverage)?;
    super::graph_wal::probe(seed, &mut coverage)?;
    super::graph_runtime::probe(seed, &mut coverage)?;
    super::graph_staging::probe(seed, &mut coverage)?;
    super::graph_binding::probe(seed, &mut coverage)?;
