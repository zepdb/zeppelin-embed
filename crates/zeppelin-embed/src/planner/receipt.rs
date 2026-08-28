//! Test-only metadata execution and feature-fault receipt controller.

use std::collections::VecDeque;
use std::sync::Mutex;

use crate::adversarial_test_support::FeatureFaultReceipt;
use crate::ingest::RowSource;
use crate::lifecycle::StoreError;
use crate::segment::{MetadataDecodeProvenance, SegmentError};

use super::{PlanFallback, SegmentBranch};

const CAMPAIGN: &str = "metadata-filter-planner";

/// One test-only query observation or fault expectation.
#[derive(Clone, Debug, Eq, PartialEq)]
#[doc(hidden)]
pub enum MetadataTestArm {
    /// Observe production execution receipts without changing behavior.
    ObserveExecution { query_id: u64 },
    /// Expect one exact semantic Columns refusal at the public query seam.
    ColumnCorruption {
        query_id: u64,
        source: RowSource,
        field_class: &'static str,
        byte_offset: u64,
    },
    /// Expect one exact shortened Alive refusal at the public query seam.
    AliveBitmapTruncation {
        query_id: u64,
        source: RowSource,
        declared_rows: u32,
        declared_bytes: u32,
        observed_bytes: u32,
    },
    /// Observe one side of the exact allow-list decision boundary.
    SelectivityBoundary {
        query_id: u64,
        expected_cardinality: u64,
    },
    /// Override only the filter-specific visited-work allowance for one query.
    VisitedBudgetFallback { query_id: u64, budget: usize },
    /// Plant a false public report at the concrete executor comparison site.
    PlanReportMismatch {
        query_id: u64,
        reported: SegmentBranch,
    },
    /// Plant mismatched evaluator row spaces at the public executor boundary.
    RowCountMismatch {
        query_id: u64,
        columns: u32,
        alive: u32,
    },
}

impl MetadataTestArm {
    const fn query_id(&self) -> u64 {
        match self {
            Self::ObserveExecution { query_id }
            | Self::ColumnCorruption { query_id, .. }
            | Self::AliveBitmapTruncation { query_id, .. }
            | Self::SelectivityBoundary { query_id, .. }
            | Self::VisitedBudgetFallback { query_id, .. }
            | Self::PlanReportMismatch { query_id, .. }
            | Self::RowCountMismatch { query_id, .. } => *query_id,
        }
    }
}

/// Facts captured at one concrete planner execution branch.
#[derive(Clone, Debug, Eq, PartialEq)]
#[doc(hidden)]
pub struct MetadataExecutionReceipt {
    pub query_id: u64,
    pub source: RowSource,
    pub row_count: u64,
    pub filter_cardinality: u64,
    pub branch: SegmentBranch,
    pub fallback: PlanFallback,
    pub rows_examined: u64,
    pub allowed_rows_examined: u64,
    pub vectors_scored: u64,
    pub graph_nodes_visited: u64,
    pub exact_fallback_rows_examined: u64,
    pub returned_candidates: u64,
    pub ef_effective: Option<usize>,
    pub visited_budget: Option<usize>,
    pub sealed: bool,
}

/// Metadata-specific fields carried beside the shared origin receipt.
#[derive(Clone, Debug, Eq, PartialEq)]
#[doc(hidden)]
pub enum MetadataFeatureDetail {
    ColumnDecodeRefused {
        source: RowSource,
        field_class: &'static str,
        byte_offset: u64,
        error_class: String,
        provenance: MetadataDecodeProvenance,
    },
    AliveBitmapTruncationRefused {
        source: RowSource,
        declared_rows: u32,
        declared_bytes: u32,
        observed_bytes: u32,
        byte_offset: u64,
        error_class: String,
        provenance: MetadataDecodeProvenance,
    },
    SelectivityBoundaryChosen {
        source: RowSource,
        cardinality: u64,
        threshold: u64,
        branch: SegmentBranch,
    },
    VisitedBudgetFallback {
        source: RowSource,
        visited: usize,
        budget: usize,
        filter_cardinality: u64,
        exact_rows_examined: u64,
        returned: u64,
        reason: PlanFallback,
    },
}

/// One feature-fault receipt that can only originate inside production code.
#[derive(Clone, Debug, Eq, PartialEq)]
#[doc(hidden)]
pub struct MetadataFeatureReceipt {
    pub query_id: u64,
    pub origin: FeatureFaultReceipt,
    pub detail: MetadataFeatureDetail,
}

/// A fail-loud misuse or synchronization failure in the hidden controller.
#[derive(Clone, Debug, Eq, PartialEq)]
#[doc(hidden)]
pub enum MetadataControllerError {
    Synchronization,
    AlreadyArmed,
    DuplicateQueryId(u64),
    DuplicateExecution {
        query_id: u64,
        source: RowSource,
    },
    DuplicateFeature {
        query_id: u64,
        fault: &'static str,
    },
    WrongFaultSite {
        query_id: u64,
        expected: &'static str,
    },
    WrongSource {
        query_id: u64,
        expected: RowSource,
        observed: RowSource,
    },
    WrongCardinality {
        query_id: u64,
        expected: u64,
        observed: u64,
    },
    WrongErrorClass {
        query_id: u64,
        expected: &'static str,
        observed: String,
    },
    WrongGuardInput {
        query_id: u64,
        field: &'static str,
        expected: u64,
        observed: u64,
    },
}

impl std::fmt::Display for MetadataControllerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Synchronization => formatter.write_str("metadata controller synchronization"),
            Self::AlreadyArmed => formatter.write_str("metadata controller already armed"),
            Self::DuplicateQueryId(query_id) => {
                write!(formatter, "metadata controller duplicate query {query_id}")
            }
            Self::DuplicateExecution { query_id, source } => write!(
                formatter,
                "metadata controller duplicate execution query {query_id} source {source:?}"
            ),
            Self::DuplicateFeature { query_id, fault } => write!(
                formatter,
                "metadata controller duplicate feature query {query_id} fault {fault}"
            ),
            Self::WrongFaultSite { query_id, expected } => write!(
                formatter,
                "metadata controller query {query_id} did not arm {expected}"
            ),
            Self::WrongSource {
                query_id,
                expected,
                observed,
            } => write!(
                formatter,
                "metadata controller query {query_id} expected source {expected:?}, observed {observed:?}"
            ),
            Self::WrongCardinality {
                query_id,
                expected,
                observed,
            } => write!(
                formatter,
                "metadata controller query {query_id} expected cardinality {expected}, observed {observed}"
            ),
            Self::WrongErrorClass {
                query_id,
                expected,
                observed,
            } => write!(
                formatter,
                "metadata controller query {query_id} expected decoder error class {expected}, observed {observed}"
            ),
            Self::WrongGuardInput {
                query_id,
                field,
                expected,
                observed,
            } => write!(
                formatter,
                "metadata controller query {query_id} expected guard input {field}={expected}, observed {observed}"
            ),
        }
    }
}

impl std::error::Error for MetadataControllerError {}

#[derive(Default)]
struct ControllerState {
    arms: VecDeque<MetadataTestArm>,
    execution: Vec<MetadataExecutionReceipt>,
    feature: Vec<MetadataFeatureReceipt>,
}

/// Concrete hidden controller for metadata feature qualification.
#[derive(Default)]
#[doc(hidden)]
pub struct MetadataTestController {
    state: Mutex<ControllerState>,
}

impl MetadataTestController {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Arms exactly one next query.
    pub fn arm(&self, arm: MetadataTestArm) -> Result<(), MetadataControllerError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| MetadataControllerError::Synchronization)?;
        if !state.arms.is_empty() {
            return Err(MetadataControllerError::AlreadyArmed);
        }
        if state
            .execution
            .iter()
            .any(|receipt| receipt.query_id == arm.query_id())
            || state
                .feature
                .iter()
                .any(|receipt| receipt.query_id == arm.query_id())
        {
            return Err(MetadataControllerError::DuplicateQueryId(arm.query_id()));
        }
        state.arms.push_back(arm);
        Ok(())
    }

    /// Arms the threshold and threshold-plus queries as one ordered pair.
    pub fn arm_selectivity_pair(
        &self,
        first_query_id: u64,
        second_query_id: u64,
        threshold: u64,
    ) -> Result<(), MetadataControllerError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| MetadataControllerError::Synchronization)?;
        if !state.arms.is_empty() {
            return Err(MetadataControllerError::AlreadyArmed);
        }
        if first_query_id == second_query_id {
            return Err(MetadataControllerError::DuplicateQueryId(first_query_id));
        }
        state.arms.push_back(MetadataTestArm::SelectivityBoundary {
            query_id: first_query_id,
            expected_cardinality: threshold,
        });
        state.arms.push_back(MetadataTestArm::SelectivityBoundary {
            query_id: second_query_id,
            expected_cardinality: threshold.saturating_add(1),
        });
        Ok(())
    }

    /// Drains branch receipts after the public operation completes.
    pub fn drain_execution_receipts(
        &self,
    ) -> Result<Vec<MetadataExecutionReceipt>, MetadataControllerError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| MetadataControllerError::Synchronization)?;
        Ok(std::mem::take(&mut state.execution))
    }

    /// Drains feature receipts after the public operation completes.
    pub fn drain_feature_receipts(
        &self,
    ) -> Result<Vec<MetadataFeatureReceipt>, MetadataControllerError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| MetadataControllerError::Synchronization)?;
        Ok(std::mem::take(&mut state.feature))
    }

    /// Rejects an arm that no production query consumed.
    pub fn assert_no_unconsumed_arm(&self) -> Result<(), MetadataControllerError> {
        let state = self
            .state
            .lock()
            .map_err(|_| MetadataControllerError::Synchronization)?;
        if state.arms.is_empty() {
            Ok(())
        } else {
            Err(MetadataControllerError::AlreadyArmed)
        }
    }

    pub(crate) fn begin_query(
        &self,
    ) -> Result<Option<MetadataQueryContext>, MetadataControllerError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| MetadataControllerError::Synchronization)?;
        Ok(state
            .arms
            .pop_front()
            .map(|arm| MetadataQueryContext { arm }))
    }

    pub(crate) fn record_execution(
        &self,
        receipt: MetadataExecutionReceipt,
    ) -> Result<(), MetadataControllerError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| MetadataControllerError::Synchronization)?;
        if state
            .execution
            .iter()
            .any(|prior| prior.query_id == receipt.query_id && prior.source == receipt.source)
        {
            return Err(MetadataControllerError::DuplicateExecution {
                query_id: receipt.query_id,
                source: receipt.source,
            });
        }
        state.execution.push(receipt);
        Ok(())
    }

    fn record_feature(
        &self,
        receipt: MetadataFeatureReceipt,
    ) -> Result<(), MetadataControllerError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| MetadataControllerError::Synchronization)?;
        if state.feature.iter().any(|prior| {
            prior.query_id == receipt.query_id && prior.origin.fault() == receipt.origin.fault()
        }) {
            return Err(MetadataControllerError::DuplicateFeature {
                query_id: receipt.query_id,
                fault: receipt.origin.fault(),
            });
        }
        state.feature.push(receipt);
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct MetadataQueryContext {
    arm: MetadataTestArm,
}

impl MetadataQueryContext {
    pub(crate) const fn query_id(&self) -> u64 {
        self.arm.query_id()
    }

    pub(crate) const fn visited_budget_override(&self) -> Option<usize> {
        match self.arm {
            MetadataTestArm::VisitedBudgetFallback { budget, .. } => Some(budget),
            _ => None,
        }
    }

    pub(crate) const fn reported_branch(&self, actual: SegmentBranch) -> SegmentBranch {
        match self.arm {
            MetadataTestArm::PlanReportMismatch { reported, .. } => reported,
            _ => actual,
        }
    }

    pub(crate) fn evaluator_alive(
        &self,
        columns: u32,
        actual: crate::meta::AliveSet,
    ) -> Result<crate::meta::AliveSet, MetadataControllerError> {
        let MetadataTestArm::RowCountMismatch {
            columns: expected_columns,
            alive: planted_alive,
            ..
        } = self.arm
        else {
            return Ok(actual);
        };
        for (field, expected, observed) in [
            (
                "columns_before_row_count_plant",
                u64::from(expected_columns),
                u64::from(columns),
            ),
            (
                "alive_before_row_count_plant",
                u64::from(expected_columns),
                u64::from(actual.row_count()),
            ),
        ] {
            if expected != observed {
                return Err(MetadataControllerError::WrongGuardInput {
                    query_id: self.query_id(),
                    field,
                    expected,
                    observed,
                });
            }
        }
        Ok(crate::meta::AliveSet::new(planted_alive))
    }

    pub(crate) fn record_column_refusal(
        &self,
        controller: &MetadataTestController,
        source: RowSource,
        error: &StoreError,
    ) -> Result<(), MetadataControllerError> {
        let MetadataTestArm::ColumnCorruption {
            source: expected,
            field_class,
            byte_offset,
            ..
        } = self.arm
        else {
            return Ok(());
        };
        if source != expected {
            return Err(MetadataControllerError::WrongSource {
                query_id: self.query_id(),
                expected,
                observed: source,
            });
        }
        let StoreError::Segment(SegmentError::MetadataSemantic { detail, provenance }) = error
        else {
            return Err(MetadataControllerError::WrongErrorClass {
                query_id: self.query_id(),
                expected: field_class,
                observed: error.to_string(),
            });
        };
        let (observed_field_class, observed_byte_offset) = match provenance {
            MetadataDecodeProvenance::ColumnsPresenceTail { byte_offset, .. } => {
                ("presence-tail", *byte_offset)
            }
            MetadataDecodeProvenance::ColumnsDictionaryCode { byte_offset, .. } => {
                ("dictionary-code", *byte_offset)
            }
            MetadataDecodeProvenance::ColumnsRawStringLength { byte_offset, .. } => {
                ("raw-string-length", *byte_offset)
            }
            MetadataDecodeProvenance::AliveBitmapTruncation { .. } => {
                return Err(MetadataControllerError::WrongErrorClass {
                    query_id: self.query_id(),
                    expected: field_class,
                    observed: "alive-bitmap-truncation".to_owned(),
                });
            }
        };
        if observed_field_class != field_class {
            return Err(MetadataControllerError::WrongErrorClass {
                query_id: self.query_id(),
                expected: field_class,
                observed: observed_field_class.to_owned(),
            });
        }
        if observed_byte_offset != byte_offset {
            return Err(MetadataControllerError::WrongGuardInput {
                query_id: self.query_id(),
                field: "byte_offset",
                expected: byte_offset,
                observed: observed_byte_offset,
            });
        }
        let error_class = detail.clone();
        let effect = format!(
            "source={source:?} field={field_class} offset={byte_offset} error={error_class}"
        );
        controller.record_feature(MetadataFeatureReceipt {
            query_id: self.query_id(),
            origin: FeatureFaultReceipt::new(
                CAMPAIGN,
                "metadata_columns_roundtrip",
                "column-corruption",
                "planner.exec.query_columns.refusal",
                1,
                effect,
            ),
            detail: MetadataFeatureDetail::ColumnDecodeRefused {
                source,
                field_class,
                byte_offset: observed_byte_offset,
                error_class,
                provenance: provenance.clone(),
            },
        })
    }

    pub(crate) fn record_alive_refusal(
        &self,
        controller: &MetadataTestController,
        source: RowSource,
        error: &StoreError,
    ) -> Result<(), MetadataControllerError> {
        let MetadataTestArm::AliveBitmapTruncation {
            source: expected,
            declared_rows,
            declared_bytes,
            observed_bytes,
            ..
        } = self.arm
        else {
            return Ok(());
        };
        if source != expected {
            return Err(MetadataControllerError::WrongSource {
                query_id: self.query_id(),
                expected,
                observed: source,
            });
        }
        let StoreError::Segment(SegmentError::MetadataSemantic { detail, provenance }) = error
        else {
            return Err(MetadataControllerError::WrongErrorClass {
                query_id: self.query_id(),
                expected: "alive-bitmap-truncation",
                observed: error.to_string(),
            });
        };
        let MetadataDecodeProvenance::AliveBitmapTruncation {
            row_count,
            byte_offset,
            declared_bytes: actual_declared_bytes,
            observed_bytes: actual_observed_bytes,
        } = provenance
        else {
            return Err(MetadataControllerError::WrongErrorClass {
                query_id: self.query_id(),
                expected: "alive-bitmap-truncation",
                observed: format!("{provenance:?}"),
            });
        };
        for (field, expected, observed) in [
            ("row_count", u64::from(declared_rows), u64::from(*row_count)),
            (
                "declared_bytes",
                u64::from(declared_bytes),
                u64::from(*actual_declared_bytes),
            ),
            (
                "observed_bytes",
                u64::from(observed_bytes),
                u64::from(*actual_observed_bytes),
            ),
        ] {
            if expected != observed {
                return Err(MetadataControllerError::WrongGuardInput {
                    query_id: self.query_id(),
                    field,
                    expected,
                    observed,
                });
            }
        }
        let error_class = detail.clone();
        let effect = format!(
            "source={source:?} rows={row_count} byte_offset={byte_offset} declared_bytes={actual_declared_bytes} observed_bytes={actual_observed_bytes} error={error_class}"
        );
        controller.record_feature(MetadataFeatureReceipt {
            query_id: self.query_id(),
            origin: FeatureFaultReceipt::new(
                CAMPAIGN,
                "metadata_bitmap_algebra",
                "bitmap-truncation",
                "planner.exec.query_alive.refusal",
                1,
                effect,
            ),
            detail: MetadataFeatureDetail::AliveBitmapTruncationRefused {
                source,
                declared_rows: *row_count,
                declared_bytes: *actual_declared_bytes,
                observed_bytes: *actual_observed_bytes,
                byte_offset: *byte_offset,
                error_class,
                provenance: provenance.clone(),
            },
        })
    }

    pub(crate) fn record_selectivity(
        &self,
        controller: &MetadataTestController,
        source: RowSource,
        cardinality: u64,
        threshold: u64,
        branch: SegmentBranch,
    ) -> Result<(), MetadataControllerError> {
        let MetadataTestArm::SelectivityBoundary {
            expected_cardinality,
            ..
        } = self.arm
        else {
            return Ok(());
        };
        if cardinality != expected_cardinality {
            return Err(MetadataControllerError::WrongCardinality {
                query_id: self.query_id(),
                expected: expected_cardinality,
                observed: cardinality,
            });
        }
        let effect = format!(
            "source={source:?} cardinality={cardinality} threshold={threshold} branch={branch:?}"
        );
        controller.record_feature(MetadataFeatureReceipt {
            query_id: self.query_id(),
            origin: FeatureFaultReceipt::new(
                CAMPAIGN,
                "metadata_execution_truth",
                "selectivity-boundary",
                "planner.choose.allow-list-threshold",
                1,
                effect,
            ),
            detail: MetadataFeatureDetail::SelectivityBoundaryChosen {
                source,
                cardinality,
                threshold,
                branch,
            },
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record_visited_fallback(
        &self,
        controller: &MetadataTestController,
        source: RowSource,
        visited: usize,
        budget: usize,
        filter_cardinality: u64,
        exact_rows_examined: u64,
        returned: u64,
        reason: PlanFallback,
    ) -> Result<(), MetadataControllerError> {
        if !matches!(self.arm, MetadataTestArm::VisitedBudgetFallback { .. }) {
            return Ok(());
        }
        let effect = format!(
            "source={source:?} visited={visited} budget={budget} filter_cardinality={filter_cardinality} exact_rows_examined={exact_rows_examined} returned={returned} reason={reason:?}"
        );
        controller.record_feature(MetadataFeatureReceipt {
            query_id: self.query_id(),
            origin: FeatureFaultReceipt::new(
                CAMPAIGN,
                "metadata_execution_truth",
                "visited-budget-fallback",
                "planner.exec.filtered-graph.visited-budget-fallback",
                1,
                effect,
            ),
            detail: MetadataFeatureDetail::VisitedBudgetFallback {
                source,
                visited,
                budget,
                filter_cardinality,
                exact_rows_examined,
                returned,
                reason,
            },
        })
    }
}

#[allow(clippy::expect_used)]
#[cfg(test)]
mod tests {
    use super::*;

    fn sealed(value: u8) -> RowSource {
        RowSource::Sealed(crate::segment::SegmentId::new(
            u64::from(value),
            [value; 10],
        ))
    }

    #[test]
    fn duplicate_execution_receipt_is_rejected() {
        let controller = MetadataTestController::new();
        let receipt = MetadataExecutionReceipt {
            query_id: 39,
            source: RowSource::Active,
            row_count: 0,
            filter_cardinality: 0,
            branch: SegmentBranch::ExactAllowList,
            fallback: PlanFallback::None,
            rows_examined: 0,
            allowed_rows_examined: 0,
            vectors_scored: 0,
            graph_nodes_visited: 0,
            exact_fallback_rows_examined: 0,
            returned_candidates: 0,
            ef_effective: None,
            visited_budget: None,
            sealed: false,
        };
        controller
            .record_execution(receipt.clone())
            .expect("first receipt");
        assert!(matches!(
            controller.record_execution(receipt),
            Err(MetadataControllerError::DuplicateExecution { .. })
        ));
    }

    #[test]
    fn every_metadata_feature_receipt_originates_at_its_production_site() {
        let controller = MetadataTestController::new();
        controller
            .arm(MetadataTestArm::ColumnCorruption {
                query_id: 1,
                source: sealed(7),
                field_class: "dictionary-code",
                byte_offset: 41,
            })
            .expect("arm column corruption");
        let column = controller
            .begin_query()
            .expect("controller")
            .expect("column context");
        column
            .record_column_refusal(
                &controller,
                sealed(7),
                &StoreError::Segment(SegmentError::MetadataSemantic {
                    detail: "dictionary code 1 out of range".to_owned(),
                    provenance: MetadataDecodeProvenance::ColumnsDictionaryCode {
                        column_id: 5,
                        row: 3,
                        byte_offset: 41,
                        code: 1,
                        dictionary_cardinality: 1,
                    },
                }),
            )
            .expect("record column refusal");

        controller
            .arm(MetadataTestArm::AliveBitmapTruncation {
                query_id: 2,
                source: sealed(8),
                declared_rows: 10,
                declared_bytes: 2,
                observed_bytes: 1,
            })
            .expect("arm alive truncation");
        let alive = controller
            .begin_query()
            .expect("controller")
            .expect("alive context");
        alive
            .record_alive_refusal(
                &controller,
                sealed(8),
                &StoreError::Segment(SegmentError::MetadataSemantic {
                    detail: "alive truncated at 8 for 2 bytes".to_owned(),
                    provenance: MetadataDecodeProvenance::AliveBitmapTruncation {
                        row_count: 10,
                        byte_offset: 8,
                        declared_bytes: 2,
                        observed_bytes: 1,
                    },
                }),
            )
            .expect("record alive refusal");

        controller
            .arm_selectivity_pair(3, 4, 64)
            .expect("arm selectivity pair");
        for (source, cardinality, branch) in [
            (RowSource::Active, 64, SegmentBranch::ExactAllowList),
            (RowSource::Active, 65, SegmentBranch::MaskedScan),
        ] {
            let query = controller
                .begin_query()
                .expect("controller")
                .expect("selectivity context");
            query
                .record_selectivity(&controller, source, cardinality, 64, branch)
                .expect("record selectivity");
        }

        controller
            .arm(MetadataTestArm::VisitedBudgetFallback {
                query_id: 5,
                budget: 1,
            })
            .expect("arm visited budget");
        let visited = controller
            .begin_query()
            .expect("controller")
            .expect("visited context");
        visited
            .record_visited_fallback(
                &controller,
                sealed(9),
                2,
                1,
                65,
                65,
                10,
                PlanFallback::VisitedBudget,
            )
            .expect("record completed fallback");

        let receipts = controller.drain_feature_receipts().expect("drain receipts");
        assert_eq!(receipts.len(), 5);
        assert_eq!(
            receipts
                .iter()
                .map(|receipt| (
                    receipt.origin.fault(),
                    receipt.origin.site(),
                    receipt.origin.cardinality(),
                ))
                .collect::<Vec<_>>(),
            vec![
                ("column-corruption", "planner.exec.query_columns.refusal", 1,),
                ("bitmap-truncation", "planner.exec.query_alive.refusal", 1,),
                (
                    "selectivity-boundary",
                    "planner.choose.allow-list-threshold",
                    1,
                ),
                (
                    "selectivity-boundary",
                    "planner.choose.allow-list-threshold",
                    1,
                ),
                (
                    "visited-budget-fallback",
                    "planner.exec.filtered-graph.visited-budget-fallback",
                    1,
                ),
            ]
        );
        controller
            .assert_no_unconsumed_arm()
            .expect("all arms consumed");
    }

    #[test]
    fn column_fault_refuses_a_decoder_error_from_the_wrong_guard() {
        let controller = MetadataTestController::new();
        controller
            .arm(MetadataTestArm::ColumnCorruption {
                query_id: 9,
                source: sealed(9),
                field_class: "dictionary-code",
                byte_offset: 90,
            })
            .expect("arm exact dictionary guard");
        let query = controller
            .begin_query()
            .expect("controller")
            .expect("column context");
        assert!(matches!(
            query.record_column_refusal(
                &controller,
                sealed(9),
                &StoreError::Segment(SegmentError::MetadataSemantic {
                    detail: "non-zero presence tail padding".to_owned(),
                    provenance: MetadataDecodeProvenance::ColumnsPresenceTail {
                        column_id: 5,
                        row_count: 10,
                        byte_offset: 90,
                        observed_byte: 0x80,
                        allowed_mask: 0x03,
                    },
                })
            ),
            Err(MetadataControllerError::WrongErrorClass {
                expected: "dictionary-code",
                ..
            })
        ));
    }
}
