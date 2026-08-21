//! Closed structured-predicate AST and bitmap evaluation over typed columns.

use super::alive::AliveSet;
use super::bitmap::DocBitmap;
use super::columns::{Column, ColumnStore, DictionaryColumn, RawStringColumn};
use super::{ColumnId, ColumnType};

/// A typed scalar carried by an equality, membership, or range predicate.
#[derive(Clone, Debug, PartialEq)]
pub enum PredicateValue {
    /// An unsigned 64-bit integer.
    U64(u64),
    /// A signed 64-bit integer.
    I64(i64),
    /// An IEEE-754 64-bit floating-point value.
    F64(f64),
    /// A Boolean value.
    Bool(bool),
    /// A UTF-8 string value.
    String(String),
}

impl PredicateValue {
    fn value_type(&self) -> ColumnType {
        match self {
            Self::U64(_) => ColumnType::U64,
            Self::I64(_) => ColumnType::I64,
            Self::F64(_) => ColumnType::F64,
            Self::Bool(_) => ColumnType::Bool,
            Self::String(_) => ColumnType::RawString,
        }
    }
}

/// One inclusive or exclusive endpoint of a numeric range.
#[derive(Clone, Debug, PartialEq)]
pub struct RangeBound {
    /// The typed numeric endpoint.
    pub value: PredicateValue,
    /// Whether a row exactly equal to the endpoint matches.
    pub inclusive: bool,
}

impl RangeBound {
    /// Creates an inclusive endpoint.
    #[must_use]
    pub fn inclusive(value: PredicateValue) -> Self {
        Self {
            value,
            inclusive: true,
        }
    }

    /// Creates an exclusive endpoint.
    #[must_use]
    pub fn exclusive(value: PredicateValue) -> Self {
        Self {
            value,
            inclusive: false,
        }
    }
}

/// A two-sided or open numeric range over one typed column.
#[derive(Clone, Debug, PartialEq)]
pub struct RangePredicate {
    /// The numeric schema column.
    pub column: ColumnId,
    /// The optional lower endpoint.
    pub lower: Option<RangeBound>,
    /// The optional upper endpoint.
    pub upper: Option<RangeBound>,
}

/// The complete v1 metadata predicate language.
///
/// This enum intentionally has no free-form or unsupported marker. Bindings
/// must reject LIKE, regex, GLOB, and every other operation not represented by
/// one of these variants before calling the engine.
#[derive(Clone, Debug, PartialEq)]
pub enum Predicate {
    /// Exact typed equality.
    Eq {
        /// The schema column.
        column: ColumnId,
        /// The typed query value.
        value: PredicateValue,
    },
    /// Membership in a typed set. An empty set matches no rows.
    In {
        /// The schema column.
        column: ColumnId,
        /// Typed query values.
        values: Vec<PredicateValue>,
    },
    /// A numeric range.
    Range(RangePredicate),
    /// Rows in which a column is non-null.
    Exists(ColumnId),
    /// Rows in which a column is null.
    IsNull(ColumnId),
    /// Logical conjunction. The empty conjunction is true within the alive set.
    And(Vec<Predicate>),
    /// Logical disjunction. The empty disjunction is false.
    Or(Vec<Predicate>),
    /// Logical negation bounded by the current alive set.
    Not(Box<Predicate>),
}

/// A typed predicate-evaluation failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EvalError {
    /// The alive set and column store describe different document spaces.
    RowCountMismatch {
        /// Number of metadata rows.
        columns: u32,
        /// Number of rows tracked by the alive set.
        alive: u32,
    },
    /// The predicate referred to a column absent from the schema.
    UnknownColumn(ColumnId),
    /// A query value did not match the declared column type.
    TypeMismatch {
        /// The mismatched column.
        column: ColumnId,
        /// The declared physical type.
        expected: ColumnType,
        /// The supplied query value type.
        actual: ColumnType,
    },
    /// Range predicates are limited to numeric columns.
    RangeRequiresNumericColumn(ColumnId),
}

impl std::fmt::Display for EvalError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RowCountMismatch { columns, alive } => write!(
                formatter,
                "column row count {columns} differs from alive row count {alive}"
            ),
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
        }
    }
}

impl std::error::Error for EvalError {}

/// Evaluates a structured predicate into an exact alive-bounded bitmap.
pub fn evaluate(
    predicate: &Predicate,
    columns: &ColumnStore,
    alive: &AliveSet,
) -> Result<DocBitmap, EvalError> {
    if columns.row_count() != alive.row_count() {
        return Err(EvalError::RowCountMismatch {
            columns: columns.row_count(),
            alive: alive.row_count(),
        });
    }
    evaluate_in_scope(predicate, columns, alive.alive_bitmap())
}

fn evaluate_in_scope(
    predicate: &Predicate,
    columns: &ColumnStore,
    scope: &DocBitmap,
) -> Result<DocBitmap, EvalError> {
    match predicate {
        Predicate::Eq { column, value } => eval_eq(*column, value, columns, scope),
        Predicate::In { column, values } => eval_in(*column, values, columns, scope),
        Predicate::Range(range) => eval_range(range, columns, scope),
        Predicate::Exists(column) => {
            let column = get_column(columns, *column)?;
            let mut result = column.present().clone();
            result.intersect_with(scope);
            Ok(result)
        }
        Predicate::IsNull(column) => {
            let column = get_column(columns, *column)?;
            let mut result = scope.clone();
            result.subtract(column.present());
            Ok(result)
        }
        Predicate::And(children) => eval_and(children, columns, scope),
        Predicate::Or(children) => {
            let mut result = DocBitmap::new();
            for child in children {
                let mut remaining = scope.clone();
                remaining.subtract(&result);
                if remaining.is_empty() {
                    break;
                }
                let child_result = evaluate_in_scope(child, columns, &remaining)?;
                result.union_with(&child_result);
            }
            Ok(result)
        }
        Predicate::Not(child) => {
            let child_result = evaluate_in_scope(child, columns, scope)?;
            let mut result = scope.clone();
            result.subtract(&child_result);
            Ok(result)
        }
    }
}

fn eval_and(
    children: &[Predicate],
    columns: &ColumnStore,
    scope: &DocBitmap,
) -> Result<DocBitmap, EvalError> {
    let mut ordered = children.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|child| estimate_cardinality(child, columns, scope));

    let mut result = scope.clone();
    for child in ordered {
        if result.is_empty() {
            break;
        }
        result = evaluate_in_scope(child, columns, &result)?;
    }
    Ok(result)
}

fn estimate_cardinality(predicate: &Predicate, columns: &ColumnStore, scope: &DocBitmap) -> u64 {
    let scope_count = scope.cardinality();
    match predicate {
        Predicate::Eq { .. } => scope_count.div_ceil(16),
        Predicate::In { values, .. } => scope_count
            .saturating_mul(u64::try_from(values.len()).unwrap_or(u64::MAX))
            .div_ceil(16)
            .min(scope_count),
        Predicate::Range(_) => scope_count / 2,
        Predicate::Exists(column) => columns.column(*column).map_or(scope_count, |column| {
            let mut present = column.present().clone();
            present.intersect_with(scope);
            present.cardinality()
        }),
        Predicate::IsNull(column) => columns.column(*column).map_or(scope_count, |column| {
            let mut missing = scope.clone();
            missing.subtract(column.present());
            missing.cardinality()
        }),
        Predicate::And(children) => children
            .iter()
            .map(|child| estimate_cardinality(child, columns, scope))
            .min()
            .unwrap_or(scope_count),
        Predicate::Or(children) => children
            .iter()
            .map(|child| estimate_cardinality(child, columns, scope))
            .fold(0_u64, u64::saturating_add)
            .min(scope_count),
        Predicate::Not(_) => scope_count / 2,
    }
}

fn get_column(columns: &ColumnStore, id: ColumnId) -> Result<&Column, EvalError> {
    columns.column(id).ok_or(EvalError::UnknownColumn(id))
}

fn eval_eq(
    id: ColumnId,
    query: &PredicateValue,
    columns: &ColumnStore,
    scope: &DocBitmap,
) -> Result<DocBitmap, EvalError> {
    let column = get_column(columns, id)?;
    ensure_type(id, column, query)?;
    let result = match (column, query) {
        (Column::U64(column), PredicateValue::U64(query)) => {
            eval_values_eq(column.values(), column.present(), scope, query)
        }
        (Column::I64(column), PredicateValue::I64(query)) => {
            eval_values_eq(column.values(), column.present(), scope, query)
        }
        (Column::F64(column), PredicateValue::F64(query)) => {
            eval_values_eq(column.values(), column.present(), scope, query)
        }
        (Column::Bool(column), PredicateValue::Bool(query)) => {
            let byte = u8::from(*query);
            eval_values_eq(column.values(), column.present(), scope, &byte)
        }
        (Column::DictionaryString(column), PredicateValue::String(query)) => {
            eval_dictionary_eq(column, query, scope)
        }
        (Column::RawString(column), PredicateValue::String(query)) => {
            eval_raw_string_eq(column, query, scope)
        }
        _ => DocBitmap::new(),
    };
    Ok(result)
}

fn eval_values_eq<T: PartialEq>(
    values: &[T],
    present: &DocBitmap,
    scope: &DocBitmap,
    query: &T,
) -> DocBitmap {
    let mut result = DocBitmap::new();
    for (position, value) in values.iter().enumerate() {
        let Some(row) = u32::try_from(position).ok() else {
            break;
        };
        if scope.contains(row) && present.contains(row) && value == query {
            result.insert(row);
        }
    }
    result
}

fn eval_dictionary_eq(column: &DictionaryColumn, query: &str, scope: &DocBitmap) -> DocBitmap {
    let Some(query_code) = column.dictionary().find(query) else {
        return DocBitmap::new();
    };
    let mut result = DocBitmap::new();
    for (position, code) in column.codes().iter().enumerate() {
        let Some(row) = u32::try_from(position).ok() else {
            break;
        };
        if scope.contains(row) && column.present().contains(row) && code == query_code {
            result.insert(row);
        }
    }
    result
}

fn eval_raw_string_eq(column: &RawStringColumn, query: &str, scope: &DocBitmap) -> DocBitmap {
    let mut result = DocBitmap::new();
    for position in 0..column.len() {
        let Some(row) = u32::try_from(position).ok() else {
            break;
        };
        if scope.contains(row)
            && column.present().contains(row)
            && column.physical_get(position) == Some(query)
        {
            result.insert(row);
        }
    }
    result
}

fn eval_in(
    id: ColumnId,
    queries: &[PredicateValue],
    columns: &ColumnStore,
    scope: &DocBitmap,
) -> Result<DocBitmap, EvalError> {
    let column = get_column(columns, id)?;
    for query in queries {
        ensure_type(id, column, query)?;
    }
    let mut result = DocBitmap::new();
    for query in queries {
        result.union_with(&eval_eq(id, query, columns, scope)?);
    }
    Ok(result)
}

fn ensure_type(id: ColumnId, column: &Column, query: &PredicateValue) -> Result<(), EvalError> {
    let expected = column.column_type();
    let compatible = matches!(
        (expected, query),
        (ColumnType::U64, PredicateValue::U64(_))
            | (ColumnType::I64, PredicateValue::I64(_))
            | (ColumnType::F64, PredicateValue::F64(_))
            | (ColumnType::Bool, PredicateValue::Bool(_))
            | (
                ColumnType::DictionaryString | ColumnType::RawString,
                PredicateValue::String(_)
            )
    );
    if compatible {
        Ok(())
    } else {
        Err(EvalError::TypeMismatch {
            column: id,
            expected,
            actual: query.value_type(),
        })
    }
}

fn eval_range(
    range: &RangePredicate,
    columns: &ColumnStore,
    scope: &DocBitmap,
) -> Result<DocBitmap, EvalError> {
    let column = get_column(columns, range.column)?;
    match column {
        Column::U64(column) => {
            let lower = u64_bound(range.column, range.lower.as_ref())?;
            let upper = u64_bound(range.column, range.upper.as_ref())?;
            Ok(eval_ordered_range(
                column.values(),
                column.present(),
                scope,
                lower,
                upper,
            ))
        }
        Column::I64(column) => {
            let lower = i64_bound(range.column, range.lower.as_ref())?;
            let upper = i64_bound(range.column, range.upper.as_ref())?;
            Ok(eval_ordered_range(
                column.values(),
                column.present(),
                scope,
                lower,
                upper,
            ))
        }
        Column::F64(column) => {
            let lower = f64_bound(range.column, range.lower.as_ref())?;
            let upper = f64_bound(range.column, range.upper.as_ref())?;
            Ok(eval_f64_range(
                column.values(),
                column.present(),
                scope,
                lower,
                upper,
            ))
        }
        Column::Bool(_) | Column::DictionaryString(_) | Column::RawString(_) => {
            Err(EvalError::RangeRequiresNumericColumn(range.column))
        }
    }
}

fn u64_bound(
    column: ColumnId,
    bound: Option<&RangeBound>,
) -> Result<Option<(u64, bool)>, EvalError> {
    bound
        .map(|bound| match bound.value {
            PredicateValue::U64(value) => Ok((value, bound.inclusive)),
            _ => Err(type_error(column, ColumnType::U64, &bound.value)),
        })
        .transpose()
}

fn i64_bound(
    column: ColumnId,
    bound: Option<&RangeBound>,
) -> Result<Option<(i64, bool)>, EvalError> {
    bound
        .map(|bound| match bound.value {
            PredicateValue::I64(value) => Ok((value, bound.inclusive)),
            _ => Err(type_error(column, ColumnType::I64, &bound.value)),
        })
        .transpose()
}

fn f64_bound(
    column: ColumnId,
    bound: Option<&RangeBound>,
) -> Result<Option<(f64, bool)>, EvalError> {
    bound
        .map(|bound| match bound.value {
            PredicateValue::F64(value) => Ok((value, bound.inclusive)),
            _ => Err(type_error(column, ColumnType::F64, &bound.value)),
        })
        .transpose()
}

fn type_error(column: ColumnId, expected: ColumnType, value: &PredicateValue) -> EvalError {
    EvalError::TypeMismatch {
        column,
        expected,
        actual: value.value_type(),
    }
}

fn eval_ordered_range<T: Copy + PartialOrd>(
    values: &[T],
    present: &DocBitmap,
    scope: &DocBitmap,
    lower: Option<(T, bool)>,
    upper: Option<(T, bool)>,
) -> DocBitmap {
    let mut result = DocBitmap::new();
    for (position, value) in values.iter().copied().enumerate() {
        let Some(row) = u32::try_from(position).ok() else {
            break;
        };
        if scope.contains(row)
            && present.contains(row)
            && lower_matches(value, lower)
            && upper_matches(value, upper)
        {
            result.insert(row);
        }
    }
    result
}

fn lower_matches<T: Copy + PartialOrd>(value: T, bound: Option<(T, bool)>) -> bool {
    bound.is_none_or(|(endpoint, inclusive)| {
        if inclusive {
            value >= endpoint
        } else {
            value > endpoint
        }
    })
}

fn upper_matches<T: Copy + PartialOrd>(value: T, bound: Option<(T, bool)>) -> bool {
    bound.is_none_or(|(endpoint, inclusive)| {
        if inclusive {
            value <= endpoint
        } else {
            value < endpoint
        }
    })
}

fn eval_f64_range(
    values: &[f64],
    present: &DocBitmap,
    scope: &DocBitmap,
    lower: Option<(f64, bool)>,
    upper: Option<(f64, bool)>,
) -> DocBitmap {
    if lower.is_some_and(|(value, _)| value.is_nan())
        || upper.is_some_and(|(value, _)| value.is_nan())
    {
        return DocBitmap::new();
    }
    let mut result = DocBitmap::new();
    for (position, value) in values.iter().copied().enumerate() {
        let Some(row) = u32::try_from(position).ok() else {
            break;
        };
        if !value.is_nan()
            && scope.contains(row)
            && present.contains(row)
            && lower_matches(value, lower)
            && upper_matches(value, upper)
        {
            result.insert(row);
        }
    }
    result
}

#[allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::{
        ColumnDefinition, ColumnInput, ColumnStoreBuilder, ColumnValue, Schema, TIMESTAMP_COLUMN,
    };
    use proptest::prelude::*;
    use proptest::test_runner::{Config, RngSeed, TestRunner};
    use rand::RngCore;

    #[derive(Clone, Copy, Debug)]
    enum ModelType {
        U64,
        I64,
        F64,
        Bool,
        DictionaryString,
        RawString,
    }

    impl ModelType {
        fn column_type(self) -> ColumnType {
            match self {
                Self::U64 => ColumnType::U64,
                Self::I64 => ColumnType::I64,
                Self::F64 => ColumnType::F64,
                Self::Bool => ColumnType::Bool,
                Self::DictionaryString => ColumnType::DictionaryString,
                Self::RawString => ColumnType::RawString,
            }
        }

        fn is_numeric(self) -> bool {
            matches!(self, Self::U64 | Self::I64 | Self::F64)
        }
    }

    #[derive(Clone, Debug)]
    struct CellSeed {
        present: bool,
        unsigned: u64,
        signed: i64,
        float: f64,
        boolean: bool,
        string: String,
    }

    impl Arbitrary for CellSeed {
        type Parameters = ();
        type Strategy = BoxedStrategy<Self>;

        fn arbitrary_with((): Self::Parameters) -> Self::Strategy {
            (
                any::<bool>(),
                any::<u64>(),
                any::<i64>(),
                prop_oneof![16 => any::<f64>(), 1 => Just(f64::NAN)],
                any::<bool>(),
                "[a-z]{0,8}",
            )
                .prop_map(|(present, unsigned, signed, float, boolean, string)| Self {
                    present,
                    unsigned,
                    signed,
                    float,
                    boolean,
                    string,
                })
                .boxed()
        }
    }

    #[derive(Clone, Debug)]
    enum PredicateSeed {
        Eq(u8, CellSeed),
        In(u8, Vec<CellSeed>),
        Range {
            column: u8,
            lower: Option<(CellSeed, bool)>,
            upper: Option<(CellSeed, bool)>,
        },
        Exists(u8),
        IsNull(u8),
        And(Vec<Self>),
        Or(Vec<Self>),
        Not(Box<Self>),
    }

    impl Arbitrary for PredicateSeed {
        type Parameters = ();
        type Strategy = BoxedStrategy<Self>;

        fn arbitrary_with((): Self::Parameters) -> Self::Strategy {
            let leaf = prop_oneof![
                (any::<u8>(), any::<CellSeed>())
                    .prop_map(|(column, value)| Self::Eq(column, value)),
                (any::<u8>(), prop::collection::vec(any::<CellSeed>(), 0..=4))
                    .prop_map(|(column, values)| Self::In(column, values)),
                (
                    any::<u8>(),
                    prop::option::of((any::<CellSeed>(), any::<bool>())),
                    prop::option::of((any::<CellSeed>(), any::<bool>()))
                )
                    .prop_map(|(column, lower, upper)| Self::Range {
                        column,
                        lower,
                        upper,
                    }),
                any::<u8>().prop_map(Self::Exists),
                any::<u8>().prop_map(Self::IsNull),
            ];

            leaf.prop_recursive(4, 64, 4, |inner| {
                prop_oneof![
                    prop::collection::vec(inner.clone(), 0..=3).prop_map(Self::And),
                    prop::collection::vec(inner.clone(), 0..=3).prop_map(Self::Or),
                    inner.prop_map(|child| Self::Not(Box::new(child))),
                ]
            })
            .boxed()
        }
    }

    #[derive(Clone, Debug, PartialEq)]
    enum ModelValue {
        U64(u64),
        I64(i64),
        F64(f64),
        Bool(bool),
        String(String),
    }

    impl ModelValue {
        fn from_seed(column_type: ModelType, seed: &CellSeed) -> Option<Self> {
            if !seed.present {
                return None;
            }
            Some(match column_type {
                ModelType::U64 => Self::U64(seed.unsigned),
                ModelType::I64 => Self::I64(seed.signed),
                ModelType::F64 => Self::F64(seed.float),
                ModelType::Bool => Self::Bool(seed.boolean),
                ModelType::DictionaryString | ModelType::RawString => {
                    Self::String(seed.string.clone())
                }
            })
        }

        fn as_column_value(&self) -> ColumnValue<'_> {
            match self {
                Self::U64(value) => ColumnValue::U64(*value),
                Self::I64(value) => ColumnValue::I64(*value),
                Self::F64(value) => ColumnValue::F64(*value),
                Self::Bool(value) => ColumnValue::Bool(*value),
                Self::String(value) => ColumnValue::String(value),
            }
        }
    }

    #[derive(Clone, Debug)]
    struct ModelRow {
        timestamp: i64,
        alive: bool,
        values: Vec<Option<ModelValue>>,
    }

    #[derive(Clone, Debug)]
    struct GeneratedCase {
        types: Vec<ModelType>,
        rows: Vec<ModelRow>,
        predicate: Predicate,
    }

    impl Arbitrary for GeneratedCase {
        type Parameters = ();
        type Strategy = BoxedStrategy<Self>;

        fn arbitrary_with((): Self::Parameters) -> Self::Strategy {
            prop::collection::vec(
                prop_oneof![
                    Just(ModelType::U64),
                    Just(ModelType::I64),
                    Just(ModelType::F64),
                    Just(ModelType::Bool),
                    Just(ModelType::DictionaryString),
                    Just(ModelType::RawString),
                ],
                0..=6,
            )
            .prop_flat_map(|types| {
                let cells = prop::collection::vec(any::<CellSeed>(), types.len());
                let row = (any::<i64>(), any::<bool>(), cells);
                let rows = prop_oneof![
                    1023 => prop::collection::vec(row.clone(), 0..=128),
                    1 => prop::collection::vec(row, 9_990..=10_000),
                ];
                (Just(types), rows, any::<PredicateSeed>())
            })
            .prop_map(|(types, rows, predicate_seed)| {
                let rows = rows
                    .into_iter()
                    .map(|(timestamp, alive, cells)| ModelRow {
                        timestamp,
                        alive,
                        values: types
                            .iter()
                            .copied()
                            .zip(&cells)
                            .map(|(column_type, seed)| ModelValue::from_seed(column_type, seed))
                            .collect(),
                    })
                    .collect();
                let predicate = materialize_predicate(&predicate_seed, &types);
                GeneratedCase {
                    types,
                    rows,
                    predicate,
                }
            })
            .boxed()
        }
    }

    fn materialize_predicate(seed: &PredicateSeed, types: &[ModelType]) -> Predicate {
        match seed {
            PredicateSeed::Eq(column, value) => {
                let (id, column_type) = select_column(*column, types);
                Predicate::Eq {
                    column: id,
                    value: predicate_value_from_seed(column_type, value),
                }
            }
            PredicateSeed::In(column, values) => {
                let (id, column_type) = select_column(*column, types);
                Predicate::In {
                    column: id,
                    values: values
                        .iter()
                        .map(|value| predicate_value_from_seed(column_type, value))
                        .collect(),
                }
            }
            PredicateSeed::Range {
                column,
                lower,
                upper,
            } => {
                let (id, column_type) = select_column(*column, types);
                if !column_type.is_numeric() {
                    return Predicate::Exists(id);
                }
                Predicate::Range(RangePredicate {
                    column: id,
                    lower: lower.as_ref().map(|(value, inclusive)| RangeBound {
                        value: predicate_value_from_seed(column_type, value),
                        inclusive: *inclusive,
                    }),
                    upper: upper.as_ref().map(|(value, inclusive)| RangeBound {
                        value: predicate_value_from_seed(column_type, value),
                        inclusive: *inclusive,
                    }),
                })
            }
            PredicateSeed::Exists(column) => {
                let (id, _) = select_column(*column, types);
                Predicate::Exists(id)
            }
            PredicateSeed::IsNull(column) => {
                let (id, _) = select_column(*column, types);
                Predicate::IsNull(id)
            }
            PredicateSeed::And(children) => Predicate::And(
                children
                    .iter()
                    .map(|child| materialize_predicate(child, types))
                    .collect(),
            ),
            PredicateSeed::Or(children) => Predicate::Or(
                children
                    .iter()
                    .map(|child| materialize_predicate(child, types))
                    .collect(),
            ),
            PredicateSeed::Not(child) => {
                Predicate::Not(Box::new(materialize_predicate(child, types)))
            }
        }
    }

    fn select_column(selector: u8, types: &[ModelType]) -> (ColumnId, ModelType) {
        let column_count = types.len().saturating_add(1);
        let position = usize::from(selector) % column_count;
        if position == 0 {
            return (TIMESTAMP_COLUMN, ModelType::I64);
        }
        let model_type = types
            .get(position.saturating_sub(1))
            .copied()
            .unwrap_or(ModelType::I64);
        let id = u32::try_from(position).unwrap_or_default();
        (ColumnId::new(id), model_type)
    }

    fn predicate_value_from_seed(column_type: ModelType, seed: &CellSeed) -> PredicateValue {
        match column_type {
            ModelType::U64 => PredicateValue::U64(seed.unsigned),
            ModelType::I64 => PredicateValue::I64(seed.signed),
            ModelType::F64 => PredicateValue::F64(seed.float),
            ModelType::Bool => PredicateValue::Bool(seed.boolean),
            ModelType::DictionaryString | ModelType::RawString => {
                PredicateValue::String(seed.string.clone())
            }
        }
    }

    fn build_case(case: &GeneratedCase) -> (ColumnStore, AliveSet) {
        let definitions = case
            .types
            .iter()
            .copied()
            .enumerate()
            .map(|(position, column_type)| {
                let id = u32::try_from(position.saturating_add(1)).unwrap_or_default();
                ColumnDefinition::new(
                    ColumnId::new(id),
                    format!("column_{id}"),
                    column_type.column_type(),
                    true,
                )
            })
            .collect();
        let schema = Schema::new(definitions).expect("generated schema is valid");
        let mut builder = ColumnStoreBuilder::new(schema);
        for row in &case.rows {
            let inputs = row
                .values
                .iter()
                .enumerate()
                .filter_map(|(position, value)| {
                    let value = value.as_ref()?;
                    let id = u32::try_from(position.saturating_add(1)).ok()?;
                    Some(ColumnInput {
                        column: ColumnId::new(id),
                        value: value.as_column_value(),
                    })
                })
                .collect::<Vec<_>>();
            builder
                .push_row(row.timestamp, &inputs)
                .expect("generated row matches schema");
        }
        let store = builder.finish().expect("generated store seals");
        let row_count = u32::try_from(case.rows.len()).expect("at most 10k rows");
        let mut alive = AliveSet::new(row_count);
        for (position, row) in case.rows.iter().enumerate() {
            if !row.alive {
                let id = u32::try_from(position).expect("at most 10k rows");
                alive
                    .tombstone(id)
                    .expect("generated tombstone is in range");
            }
        }
        (store, alive)
    }

    fn naive_matches(predicate: &Predicate, row: &ModelRow) -> bool {
        match predicate {
            Predicate::Eq { column, value } => {
                row_value(row, *column).is_some_and(|stored| model_eq(&stored, value))
            }
            Predicate::In { column, values } => row_value(row, *column)
                .is_some_and(|stored| values.iter().any(|query| model_eq(&stored, query))),
            Predicate::Range(range) => row_value(row, range.column).is_some_and(|stored| {
                model_range(&stored, range.lower.as_ref(), range.upper.as_ref())
            }),
            Predicate::Exists(column) => row_value(row, *column).is_some(),
            Predicate::IsNull(column) => row_value(row, *column).is_none(),
            Predicate::And(children) => children.iter().all(|child| naive_matches(child, row)),
            Predicate::Or(children) => children.iter().any(|child| naive_matches(child, row)),
            Predicate::Not(child) => !naive_matches(child, row),
        }
    }

    fn row_value(row: &ModelRow, column: ColumnId) -> Option<ModelValue> {
        if column == TIMESTAMP_COLUMN {
            return Some(ModelValue::I64(row.timestamp));
        }
        let position = usize::try_from(column.get()).ok()?.checked_sub(1)?;
        row.values.get(position).cloned().flatten()
    }

    fn model_eq(stored: &ModelValue, query: &PredicateValue) -> bool {
        match (stored, query) {
            (ModelValue::U64(stored), PredicateValue::U64(query)) => stored == query,
            (ModelValue::I64(stored), PredicateValue::I64(query)) => stored == query,
            (ModelValue::F64(stored), PredicateValue::F64(query)) => stored == query,
            (ModelValue::Bool(stored), PredicateValue::Bool(query)) => stored == query,
            (ModelValue::String(stored), PredicateValue::String(query)) => stored == query,
            _ => false,
        }
    }

    fn model_range(
        stored: &ModelValue,
        lower: Option<&RangeBound>,
        upper: Option<&RangeBound>,
    ) -> bool {
        match stored {
            ModelValue::U64(value) => {
                model_bound_u64(*value, lower, true) && model_bound_u64(*value, upper, false)
            }
            ModelValue::I64(value) => {
                model_bound_i64(*value, lower, true) && model_bound_i64(*value, upper, false)
            }
            ModelValue::F64(value) => {
                !value.is_nan()
                    && model_bound_f64(*value, lower, true)
                    && model_bound_f64(*value, upper, false)
            }
            ModelValue::Bool(_) | ModelValue::String(_) => false,
        }
    }

    fn model_bound_u64(value: u64, bound: Option<&RangeBound>, lower: bool) -> bool {
        bound.is_none_or(|bound| match bound.value {
            PredicateValue::U64(endpoint) => compare_bound(value, endpoint, bound.inclusive, lower),
            _ => false,
        })
    }

    fn model_bound_i64(value: i64, bound: Option<&RangeBound>, lower: bool) -> bool {
        bound.is_none_or(|bound| match bound.value {
            PredicateValue::I64(endpoint) => compare_bound(value, endpoint, bound.inclusive, lower),
            _ => false,
        })
    }

    fn model_bound_f64(value: f64, bound: Option<&RangeBound>, lower: bool) -> bool {
        bound.is_none_or(|bound| match bound.value {
            PredicateValue::F64(endpoint) if !endpoint.is_nan() => {
                compare_bound(value, endpoint, bound.inclusive, lower)
            }
            _ => false,
        })
    }

    fn compare_bound<T: PartialOrd>(value: T, endpoint: T, inclusive: bool, lower: bool) -> bool {
        match (lower, inclusive) {
            (true, true) => value >= endpoint,
            (true, false) => value > endpoint,
            (false, true) => value <= endpoint,
            (false, false) => value < endpoint,
        }
    }

    fn one_i64_column(values: &[i64]) -> (ColumnStore, AliveSet) {
        let schema = Schema::new(vec![ColumnDefinition::new(
            ColumnId::new(1),
            "number",
            ColumnType::I64,
            true,
        )])
        .expect("valid schema");
        let mut builder = ColumnStoreBuilder::new(schema);
        for value in values {
            builder
                .push_row(
                    0,
                    &[ColumnInput {
                        column: ColumnId::new(1),
                        value: ColumnValue::I64(*value),
                    }],
                )
                .expect("valid i64 row");
        }
        let store = builder.finish().expect("store seals");
        let alive = AliveSet::new(store.row_count());
        (store, alive)
    }

    fn one_f64_column(values: &[f64]) -> (ColumnStore, AliveSet) {
        let schema = Schema::new(vec![ColumnDefinition::new(
            ColumnId::new(1),
            "number",
            ColumnType::F64,
            true,
        )])
        .expect("valid schema");
        let mut builder = ColumnStoreBuilder::new(schema);
        for value in values {
            builder
                .push_row(
                    0,
                    &[ColumnInput {
                        column: ColumnId::new(1),
                        value: ColumnValue::F64(*value),
                    }],
                )
                .expect("valid f64 row");
        }
        let store = builder.finish().expect("store seals");
        let alive = AliveSet::new(store.row_count());
        (store, alive)
    }

    #[test]
    fn range_bounds_are_inclusive_or_exclusive() {
        let (columns, alive) = one_i64_column(&[1, 2, 3]);
        let inclusive = Predicate::Range(RangePredicate {
            column: ColumnId::new(1),
            lower: Some(RangeBound::inclusive(PredicateValue::I64(1))),
            upper: Some(RangeBound::inclusive(PredicateValue::I64(3))),
        });
        let exclusive = Predicate::Range(RangePredicate {
            column: ColumnId::new(1),
            lower: Some(RangeBound::exclusive(PredicateValue::I64(1))),
            upper: Some(RangeBound::exclusive(PredicateValue::I64(3))),
        });

        assert_eq!(
            evaluate(&inclusive, &columns, &alive)
                .expect("valid range")
                .iter()
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert_eq!(
            evaluate(&exclusive, &columns, &alive)
                .expect("valid range")
                .iter()
                .collect::<Vec<_>>(),
            vec![1]
        );
    }

    #[test]
    fn range_handles_i64_extremes() {
        let (columns, alive) = one_i64_column(&[i64::MIN, 0, i64::MAX]);
        let predicate = Predicate::Range(RangePredicate {
            column: ColumnId::new(1),
            lower: Some(RangeBound::inclusive(PredicateValue::I64(i64::MIN))),
            upper: Some(RangeBound::inclusive(PredicateValue::I64(i64::MAX))),
        });

        assert_eq!(
            evaluate(&predicate, &columns, &alive)
                .expect("valid extreme range")
                .iter()
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
    }

    #[test]
    fn f64_nan_never_matches_ranges() {
        let (columns, alive) = one_f64_column(&[f64::NAN, 1.0]);
        let unbounded = Predicate::Range(RangePredicate {
            column: ColumnId::new(1),
            lower: None,
            upper: None,
        });

        assert_eq!(
            evaluate(&unbounded, &columns, &alive)
                .expect("valid f64 range")
                .iter()
                .collect::<Vec<_>>(),
            vec![1]
        );
    }

    #[test]
    fn empty_in_set_matches_no_rows() {
        let (columns, alive) = one_i64_column(&[1, 2]);
        let predicate = Predicate::In {
            column: ColumnId::new(1),
            values: Vec::new(),
        };

        assert_eq!(
            evaluate(&predicate, &columns, &alive)
                .expect("empty set is valid")
                .iter()
                .collect::<Vec<_>>(),
            Vec::<u32>::new()
        );
    }

    #[test]
    fn negation_is_bounded_by_alive_set() {
        let (columns, mut alive) = one_i64_column(&[1, 2, 3]);
        alive.tombstone(1).expect("row exists");
        let predicate = Predicate::Not(Box::new(Predicate::Eq {
            column: ColumnId::new(1),
            value: PredicateValue::I64(1),
        }));

        assert_eq!(
            evaluate(&predicate, &columns, &alive)
                .expect("valid negation")
                .iter()
                .collect::<Vec<_>>(),
            vec![2]
        );
    }

    fn is_closed_v1_variant(predicate: &Predicate) -> bool {
        match predicate {
            Predicate::Eq { .. }
            | Predicate::In { .. }
            | Predicate::Range(_)
            | Predicate::Exists(_)
            | Predicate::IsNull(_)
            | Predicate::And(_)
            | Predicate::Or(_)
            | Predicate::Not(_) => true,
        }
    }

    #[test]
    fn predicate_ast_is_closed_without_unsupported_variant() {
        let column = ColumnId::new(1);
        let variants = [
            Predicate::Eq {
                column,
                value: PredicateValue::I64(1),
            },
            Predicate::In {
                column,
                values: vec![PredicateValue::I64(1)],
            },
            Predicate::Range(RangePredicate {
                column,
                lower: None,
                upper: None,
            }),
            Predicate::Exists(column),
            Predicate::IsNull(column),
            Predicate::And(Vec::new()),
            Predicate::Or(Vec::new()),
            Predicate::Not(Box::new(Predicate::Exists(column))),
        ];
        assert_eq!(
            variants
                .iter()
                .map(is_closed_v1_variant)
                .collect::<Vec<_>>(),
            vec![true; 8]
        );
    }

    #[test]
    fn prop_predicate_eval_equals_naive_row_loop() {
        let name = "meta::predicate::prop_predicate_eval_equals_naive_row_loop";
        let mut seeded = crate::test_support::seeded_rng(name);
        let config = Config {
            rng_seed: RngSeed::Fixed(seeded.next_u64()),
            ..Config::default()
        };
        let mut runner = TestRunner::new(config);
        let result = runner.run(&any::<GeneratedCase>(), |case| {
            let (columns, alive) = build_case(&case);
            let actual = evaluate(&case.predicate, &columns, &alive)
                .expect("generated predicate is schema-valid")
                .iter()
                .collect::<Vec<_>>();
            let expected = case
                .rows
                .iter()
                .enumerate()
                .filter_map(|(position, row)| {
                    (row.alive && naive_matches(&case.predicate, row))
                        .then(|| u32::try_from(position).ok())
                        .flatten()
                })
                .collect::<Vec<_>>();
            prop_assert_eq!(actual, expected);
            Ok(())
        });
        assert!(result.is_ok(), "property result: {result:?}");
    }
}
