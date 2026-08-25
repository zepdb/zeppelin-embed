//! Conservative clustering-range pruning and schema validation.

use crate::meta::{ColumnId, ColumnType, Predicate, PredicateValue, Schema, TIMESTAMP_COLUMN};
use crate::segment::ClusteringKeyRange;

/// A typed planning-time predicate rejection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlanError {
    /// The predicate named a column absent from the collection schema.
    UnknownColumn(ColumnId),
    /// A predicate value disagreed with its declared column type.
    TypeMismatch {
        /// Mismatched column.
        column: ColumnId,
        /// Declared type.
        expected: ColumnType,
        /// Supplied type.
        actual: ColumnType,
    },
    /// A range was attached to a non-numeric column.
    RangeRequiresNumericColumn(ColumnId),
    /// Metadata and alive arrays described different row spaces.
    RowCountMismatch {
        /// Metadata row count.
        columns: u32,
        /// Alive row count.
        alive: u32,
    },
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownColumn(column) => write!(formatter, "unknown column {}", column.get()),
            Self::TypeMismatch {
                column,
                expected,
                actual,
            } => write!(
                formatter,
                "column {} expects {expected:?}, received {actual:?}",
                column.get()
            ),
            Self::RangeRequiresNumericColumn(column) => {
                write!(formatter, "column {} is not numeric", column.get())
            }
            Self::RowCountMismatch { columns, alive } => write!(
                formatter,
                "column row count {columns} differs from alive row count {alive}"
            ),
        }
    }
}

impl std::error::Error for PlanError {}

/// Validates every predicate node against the collection schema before execution.
pub fn validate_predicate(predicate: &Predicate, schema: &Schema) -> Result<(), PlanError> {
    match predicate {
        Predicate::Eq { column, value } => validate_value(*column, value, schema),
        Predicate::In { column, values } => {
            require_column(*column, schema)?;
            for value in values {
                validate_value(*column, value, schema)?;
            }
            Ok(())
        }
        Predicate::Range(range) => {
            let definition = require_column(range.column, schema)?;
            if !matches!(
                definition.column_type(),
                ColumnType::U64 | ColumnType::I64 | ColumnType::F64
            ) {
                return Err(PlanError::RangeRequiresNumericColumn(range.column));
            }
            if let Some(lower) = &range.lower {
                validate_value(range.column, &lower.value, schema)?;
            }
            if let Some(upper) = &range.upper {
                validate_value(range.column, &upper.value, schema)?;
            }
            Ok(())
        }
        Predicate::Exists(column) | Predicate::IsNull(column) => {
            require_column(*column, schema).map(|_| ())
        }
        Predicate::And(children) | Predicate::Or(children) => {
            for child in children {
                validate_predicate(child, schema)?;
            }
            Ok(())
        }
        Predicate::Not(child) => validate_predicate(child, schema),
    }
}

fn require_column(
    column: ColumnId,
    schema: &Schema,
) -> Result<&crate::meta::ColumnDefinition, PlanError> {
    schema
        .column(column)
        .ok_or(PlanError::UnknownColumn(column))
}

fn validate_value(
    column: ColumnId,
    value: &PredicateValue,
    schema: &Schema,
) -> Result<(), PlanError> {
    let expected = require_column(column, schema)?.column_type();
    let actual = value.value_type();
    let compatible = expected == actual
        || matches!(
            (expected, actual),
            (ColumnType::DictionaryString, ColumnType::RawString)
        );
    if compatible {
        Ok(())
    } else {
        Err(PlanError::TypeMismatch {
            column,
            expected,
            actual,
        })
    }
}

/// Returns `false` only when timestamp statistics prove no row can match.
#[must_use]
pub fn segment_may_match(range: ClusteringKeyRange, predicate: &Predicate) -> bool {
    match range {
        ClusteringKeyRange::Unstamped => true,
        ClusteringKeyRange::Empty => false,
        ClusteringKeyRange::Bounded { min_ts, max_ts } => {
            predicate_may_match_timestamp(predicate, min_ts, max_ts)
        }
    }
}

fn predicate_may_match_timestamp(predicate: &Predicate, min_ts: i64, max_ts: i64) -> bool {
    match predicate {
        Predicate::Eq { column, value } if *column == TIMESTAMP_COLUMN => {
            matches!(value, PredicateValue::I64(value) if *value >= min_ts && *value <= max_ts)
        }
        Predicate::In { column, values } if *column == TIMESTAMP_COLUMN => values.iter().any(
            |value| matches!(value, PredicateValue::I64(value) if *value >= min_ts && *value <= max_ts),
        ),
        Predicate::Range(range) if range.column == TIMESTAMP_COLUMN => {
            let lower_overlaps = range.lower.as_ref().is_none_or(|bound| {
                matches!(&bound.value, PredicateValue::I64(value) if if bound.inclusive {
                    max_ts >= *value
                } else {
                    max_ts > *value
                })
            });
            let upper_overlaps = range.upper.as_ref().is_none_or(|bound| {
                matches!(&bound.value, PredicateValue::I64(value) if if bound.inclusive {
                    min_ts <= *value
                } else {
                    min_ts < *value
                })
            });
            lower_overlaps && upper_overlaps
        }
        Predicate::Exists(column) if *column == TIMESTAMP_COLUMN => true,
        Predicate::IsNull(column) if *column == TIMESTAMP_COLUMN => false,
        Predicate::And(children) => children
            .iter()
            .all(|child| predicate_may_match_timestamp(child, min_ts, max_ts)),
        Predicate::Or(children) => children
            .iter()
            .any(|child| predicate_may_match_timestamp(child, min_ts, max_ts)),
        Predicate::Not(_) => true,
        Predicate::Eq { .. }
        | Predicate::In { .. }
        | Predicate::Range(_)
        | Predicate::Exists(_)
        | Predicate::IsNull(_) => true,
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use rand::Rng;

    use super::*;
    use crate::meta::{RangeBound, RangePredicate};

    #[test]
    fn pruning_by_clustering_range_never_drops_a_matching_segment() {
        let mut random = crate::test_support::seeded_rng(
            "planner::prune::pruning_by_clustering_range_never_drops_a_matching_segment",
        );
        for _ in 0..2_048 {
            let first = random.random_range(-100_i64..=100);
            let second = random.random_range(-100_i64..=100);
            let min_ts = first.min(second);
            let max_ts = first.max(second);
            let lower = random.random_range(-110_i64..=110);
            let upper = random.random_range(-110_i64..=110);
            let lower_inclusive = random.random();
            let upper_inclusive = random.random();
            let predicate = Predicate::Range(RangePredicate {
                column: TIMESTAMP_COLUMN,
                lower: Some(RangeBound {
                    value: PredicateValue::I64(lower),
                    inclusive: lower_inclusive,
                }),
                upper: Some(RangeBound {
                    value: PredicateValue::I64(upper),
                    inclusive: upper_inclusive,
                }),
            });
            let row_truth = (min_ts..=max_ts).any(|value| {
                (value > lower || (lower_inclusive && value == lower))
                    && (value < upper || (upper_inclusive && value == upper))
            });
            if row_truth {
                assert!(segment_may_match(
                    ClusteringKeyRange::Bounded { min_ts, max_ts },
                    &predicate
                ));
            }
            assert!(segment_may_match(ClusteringKeyRange::Unstamped, &predicate));
        }
    }
}
