//! Typed, contiguous column arrays and row-aligned builders.

use super::bitmap::DocBitmap;
use super::dict::{
    DictionaryBuilder, DictionaryCodes, DictionaryError, StringDictionary, StringStorage,
};
use super::{ColumnId, ColumnType, Schema, TIMESTAMP_COLUMN};

/// A borrowed, typed value supplied for one metadata row.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ColumnValue<'a> {
    /// An unsigned integer.
    U64(u64),
    /// A signed integer.
    I64(i64),
    /// A floating-point value.
    F64(f64),
    /// A Boolean value.
    Bool(bool),
    /// A UTF-8 value for either string physical representation.
    String(&'a str),
}

impl ColumnValue<'_> {
    fn compatible_with(self, column_type: ColumnType) -> bool {
        matches!(
            (self, column_type),
            (Self::U64(_), ColumnType::U64)
                | (Self::I64(_), ColumnType::I64)
                | (Self::F64(_), ColumnType::F64)
                | (Self::Bool(_), ColumnType::Bool)
                | (
                    Self::String(_),
                    ColumnType::DictionaryString | ColumnType::RawString
                )
        )
    }

    fn value_type(self) -> ColumnType {
        match self {
            Self::U64(_) => ColumnType::U64,
            Self::I64(_) => ColumnType::I64,
            Self::F64(_) => ColumnType::F64,
            Self::Bool(_) => ColumnType::Bool,
            Self::String(_) => ColumnType::RawString,
        }
    }
}

/// One column/value pair supplied while appending a row.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColumnInput<'a> {
    /// The schema column receiving the value.
    pub column: ColumnId,
    /// The typed value to append.
    pub value: ColumnValue<'a>,
}

/// A typed failure that leaves the logical row count unchanged.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BuildError {
    /// An input referred to an identifier absent from the schema.
    UnknownColumn(ColumnId),
    /// The same column appeared twice in one row.
    DuplicateColumn(ColumnId),
    /// A non-nullable column was omitted.
    MissingRequiredColumn(ColumnId),
    /// The timestamp must use the dedicated append argument.
    TimestampProvidedAsInput,
    /// A value did not match its declared physical type.
    TypeMismatch {
        /// The mismatched column.
        column: ColumnId,
        /// The declared physical type.
        expected: ColumnType,
        /// The supplied value type.
        actual: ColumnType,
    },
    /// The segment reached the supported u32 document-id space.
    TooManyRows,
    /// Dictionary or string side-array construction failed.
    Dictionary(DictionaryError),
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownColumn(column) => write!(formatter, "unknown column {}", column.get()),
            Self::DuplicateColumn(column) => {
                write!(formatter, "duplicate column {}", column.get())
            }
            Self::MissingRequiredColumn(column) => {
                write!(formatter, "missing required column {}", column.get())
            }
            Self::TimestampProvidedAsInput => {
                formatter.write_str("timestamp must use the dedicated ts argument")
            }
            Self::TypeMismatch {
                column,
                expected,
                actual,
            } => write!(
                formatter,
                "column {} expects {expected:?}, received {actual:?}",
                column.get()
            ),
            Self::TooManyRows => formatter.write_str("segment row count exceeds u32"),
            Self::Dictionary(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for BuildError {}

impl From<DictionaryError> for BuildError {
    fn from(error: DictionaryError) -> Self {
        Self::Dictionary(error)
    }
}

/// A nullable numeric array plus its explicit presence bitmap.
#[derive(Clone, Debug, PartialEq)]
#[repr(C)]
pub struct NumericColumn<T> {
    values: Vec<T>,
    present: DocBitmap,
}

impl<T: Copy> NumericColumn<T> {
    /// Returns the physical row count including null placeholders.
    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Returns whether the physical array is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Reads a non-null value, returning `None` for null or out-of-range rows.
    #[must_use]
    pub fn get(&self, row: u32) -> Option<T> {
        if !self.present.contains(row) {
            return None;
        }
        let position = usize::try_from(row).ok()?;
        self.values.get(position).copied()
    }

    /// Returns the presence bitmap used by `exists` and `null` predicates.
    #[must_use]
    pub fn present(&self) -> &DocBitmap {
        &self.present
    }

    pub(crate) fn values(&self) -> &[T] {
        &self.values
    }
}

/// A nullable Boolean array stored as contiguous zero/one bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct BoolColumn {
    values: Vec<u8>,
    present: DocBitmap,
}

impl BoolColumn {
    /// Returns the physical row count including null placeholders.
    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Returns whether the physical array is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Reads a non-null value, returning `None` for null or out-of-range rows.
    #[must_use]
    pub fn get(&self, row: u32) -> Option<bool> {
        if !self.present.contains(row) {
            return None;
        }
        let position = usize::try_from(row).ok()?;
        self.values.get(position).map(|value| *value != 0)
    }

    /// Returns the presence bitmap used by `exists` and `null` predicates.
    #[must_use]
    pub fn present(&self) -> &DocBitmap {
        &self.present
    }

    pub(crate) fn values(&self) -> &[u8] {
        &self.values
    }
}

/// A nullable, low-cardinality dictionary string column.
#[derive(Clone, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct DictionaryColumn {
    codes: DictionaryCodes,
    dictionary: StringDictionary,
    present: DocBitmap,
}

impl DictionaryColumn {
    /// Returns the physical row count including null placeholders.
    #[must_use]
    pub fn len(&self) -> usize {
        self.codes.len()
    }

    /// Returns whether the physical array is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.codes.is_empty()
    }

    /// Decodes a non-null row value.
    #[must_use]
    pub fn get(&self, row: u32) -> Option<&str> {
        if !self.present.contains(row) {
            return None;
        }
        let position = usize::try_from(row).ok()?;
        let code = self.codes.get(position)?;
        self.dictionary.get(code)
    }

    /// Returns the selected physical code array.
    #[must_use]
    pub fn codes(&self) -> &DictionaryCodes {
        &self.codes
    }

    /// Returns the immutable string dictionary.
    #[must_use]
    pub fn dictionary(&self) -> &StringDictionary {
        &self.dictionary
    }

    /// Returns the presence bitmap used by `exists` and `null` predicates.
    #[must_use]
    pub fn present(&self) -> &DocBitmap {
        &self.present
    }
}

/// A nullable raw UTF-8 column backed by contiguous offsets and bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct RawStringColumn {
    values: StringStorage,
    present: DocBitmap,
}

impl RawStringColumn {
    /// Returns the physical row count including null placeholders.
    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Returns whether the physical array is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.len() == 0
    }

    /// Reads a non-null row value.
    #[must_use]
    pub fn get(&self, row: u32) -> Option<&str> {
        if !self.present.contains(row) {
            return None;
        }
        let position = usize::try_from(row).ok()?;
        self.values.get(position)
    }

    /// Returns the presence bitmap used by `exists` and `null` predicates.
    #[must_use]
    pub fn present(&self) -> &DocBitmap {
        &self.present
    }

    pub(crate) fn physical_get(&self, position: usize) -> Option<&str> {
        self.values.get(position)
    }
}

/// One immutable typed physical column.
#[derive(Clone, Debug, PartialEq)]
pub enum Column {
    /// An unsigned-integer array.
    U64(NumericColumn<u64>),
    /// A signed-integer array, including `ts`.
    I64(NumericColumn<i64>),
    /// A floating-point array.
    F64(NumericColumn<f64>),
    /// A Boolean byte array.
    Bool(BoolColumn),
    /// A dictionary string array.
    DictionaryString(DictionaryColumn),
    /// A raw UTF-8 array.
    RawString(RawStringColumn),
}

impl Column {
    /// Returns the physical column type.
    #[must_use]
    pub const fn column_type(&self) -> ColumnType {
        match self {
            Self::U64(_) => ColumnType::U64,
            Self::I64(_) => ColumnType::I64,
            Self::F64(_) => ColumnType::F64,
            Self::Bool(_) => ColumnType::Bool,
            Self::DictionaryString(_) => ColumnType::DictionaryString,
            Self::RawString(_) => ColumnType::RawString,
        }
    }

    /// Returns the physical row count including null placeholders.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::U64(column) => column.len(),
            Self::I64(column) => column.len(),
            Self::F64(column) => column.len(),
            Self::Bool(column) => column.len(),
            Self::DictionaryString(column) => column.len(),
            Self::RawString(column) => column.len(),
        }
    }

    /// Returns whether the column contains no physical rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the non-null row bitmap.
    #[must_use]
    pub fn present(&self) -> &DocBitmap {
        match self {
            Self::U64(column) => column.present(),
            Self::I64(column) => column.present(),
            Self::F64(column) => column.present(),
            Self::Bool(column) => column.present(),
            Self::DictionaryString(column) => column.present(),
            Self::RawString(column) => column.present(),
        }
    }
}

/// Immutable metadata columns aligned to one segment's document-id space.
#[derive(Clone, Debug, PartialEq)]
pub struct ColumnStore {
    schema: Schema,
    columns: Vec<Column>,
    row_count: u32,
}

impl ColumnStore {
    /// Returns the schema that defines these physical arrays.
    #[must_use]
    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// Returns the shared segment row count.
    #[must_use]
    pub const fn row_count(&self) -> u32 {
        self.row_count
    }

    /// Finds an immutable physical column by typed identifier.
    #[must_use]
    pub fn column(&self, id: ColumnId) -> Option<&Column> {
        self.schema
            .columns()
            .iter()
            .zip(&self.columns)
            .find_map(|(definition, column)| (definition.id() == id).then_some(column))
    }

    /// Reads one required timestamp.
    #[must_use]
    pub fn timestamp(&self, row: u32) -> Option<i64> {
        match self.column(TIMESTAMP_COLUMN)? {
            Column::I64(column) => column.get(row),
            _ => None,
        }
    }

    /// Returns the complete required timestamp side-array.
    #[must_use]
    pub fn timestamps(&self) -> &[i64] {
        match self.column(TIMESTAMP_COLUMN) {
            Some(Column::I64(column)) => column.values(),
            _ => &[],
        }
    }
}

enum BuilderColumn {
    U64(Vec<u64>, DocBitmap),
    I64(Vec<i64>, DocBitmap),
    F64(Vec<f64>, DocBitmap),
    Bool(Vec<u8>, DocBitmap),
    Dictionary {
        codes: Vec<u32>,
        dictionary: DictionaryBuilder,
        present: DocBitmap,
    },
    RawString(StringStorage, DocBitmap),
}

impl BuilderColumn {
    fn new(column_type: ColumnType) -> Self {
        match column_type {
            ColumnType::U64 => Self::U64(Vec::new(), DocBitmap::new()),
            ColumnType::I64 => Self::I64(Vec::new(), DocBitmap::new()),
            ColumnType::F64 => Self::F64(Vec::new(), DocBitmap::new()),
            ColumnType::Bool => Self::Bool(Vec::new(), DocBitmap::new()),
            ColumnType::DictionaryString => Self::Dictionary {
                codes: Vec::new(),
                dictionary: DictionaryBuilder::new(),
                present: DocBitmap::new(),
            },
            ColumnType::RawString => Self::RawString(StringStorage::new(), DocBitmap::new()),
        }
    }

    fn preflight(&self, value: Option<ColumnValue<'_>>) -> Result<(), DictionaryError> {
        match (self, value) {
            (Self::Dictionary { dictionary, .. }, Some(ColumnValue::String(value))) => {
                dictionary.can_intern(value)
            }
            (Self::RawString(values, _), Some(ColumnValue::String(value))) => {
                values.can_push(value)
            }
            (Self::RawString(values, _), None) => values.can_push(""),
            _ => Ok(()),
        }
    }

    fn push(
        &mut self,
        row: u32,
        column: ColumnId,
        value: Option<ColumnValue<'_>>,
    ) -> Result<(), BuildError> {
        let expected = self.column_type();
        match (self, value) {
            (Self::U64(values, present), Some(ColumnValue::U64(value))) => {
                values.push(value);
                present.insert(row);
            }
            (Self::U64(values, _), None) => values.push(0),
            (Self::I64(values, present), Some(ColumnValue::I64(value))) => {
                values.push(value);
                present.insert(row);
            }
            (Self::I64(values, _), None) => values.push(0),
            (Self::F64(values, present), Some(ColumnValue::F64(value))) => {
                values.push(value);
                present.insert(row);
            }
            (Self::F64(values, _), None) => values.push(0.0),
            (Self::Bool(values, present), Some(ColumnValue::Bool(value))) => {
                values.push(u8::from(value));
                present.insert(row);
            }
            (Self::Bool(values, _), None) => values.push(0),
            (
                Self::Dictionary {
                    codes,
                    dictionary,
                    present,
                },
                Some(ColumnValue::String(value)),
            ) => {
                codes.push(dictionary.intern(value)?);
                present.insert(row);
            }
            (Self::Dictionary { codes, .. }, None) => codes.push(0),
            (Self::RawString(values, present), Some(ColumnValue::String(value))) => {
                values.push(value)?;
                present.insert(row);
            }
            (Self::RawString(values, _), None) => values.push("")?,
            (_, Some(value)) => {
                return Err(BuildError::TypeMismatch {
                    column,
                    expected,
                    actual: value.value_type(),
                });
            }
        }
        Ok(())
    }

    fn column_type(&self) -> ColumnType {
        match self {
            Self::U64(_, _) => ColumnType::U64,
            Self::I64(_, _) => ColumnType::I64,
            Self::F64(_, _) => ColumnType::F64,
            Self::Bool(_, _) => ColumnType::Bool,
            Self::Dictionary { .. } => ColumnType::DictionaryString,
            Self::RawString(_, _) => ColumnType::RawString,
        }
    }

    fn finish(self) -> Result<Column, DictionaryError> {
        match self {
            Self::U64(values, present) => Ok(Column::U64(NumericColumn { values, present })),
            Self::I64(values, present) => Ok(Column::I64(NumericColumn { values, present })),
            Self::F64(values, present) => Ok(Column::F64(NumericColumn { values, present })),
            Self::Bool(values, present) => Ok(Column::Bool(BoolColumn { values, present })),
            Self::Dictionary {
                codes,
                dictionary,
                present,
            } => {
                let cardinality = dictionary.cardinality();
                Ok(Column::DictionaryString(DictionaryColumn {
                    codes: DictionaryCodes::from_u32(codes, cardinality)?,
                    dictionary: dictionary.finish(),
                    present,
                }))
            }
            Self::RawString(values, present) => {
                Ok(Column::RawString(RawStringColumn { values, present }))
            }
        }
    }
}

/// Mutable row builder that keeps every physical column exactly aligned.
pub struct ColumnStoreBuilder {
    schema: Schema,
    columns: Vec<BuilderColumn>,
    row_count: u32,
}

impl ColumnStoreBuilder {
    /// Creates empty physical arrays for a validated schema.
    #[must_use]
    pub fn new(schema: Schema) -> Self {
        let columns = schema
            .columns()
            .iter()
            .map(|definition| BuilderColumn::new(definition.column_type()))
            .collect();
        Self {
            schema,
            columns,
            row_count: 0,
        }
    }

    /// Appends one timestamp and its typed user-column values atomically.
    pub fn push_row(
        &mut self,
        timestamp: i64,
        inputs: &[ColumnInput<'_>],
    ) -> Result<u32, BuildError> {
        if self.row_count == u32::MAX {
            return Err(BuildError::TooManyRows);
        }
        self.validate_inputs(inputs)?;

        for (definition, builder) in self.schema.columns().iter().zip(&self.columns) {
            let value = if definition.id() == TIMESTAMP_COLUMN {
                Some(ColumnValue::I64(timestamp))
            } else {
                find_input(inputs, definition.id()).map(|input| input.value)
            };
            builder.preflight(value)?;
        }

        let row = self.row_count;
        for (definition, builder) in self.schema.columns().iter().zip(&mut self.columns) {
            let value = if definition.id() == TIMESTAMP_COLUMN {
                Some(ColumnValue::I64(timestamp))
            } else {
                find_input(inputs, definition.id()).map(|input| input.value)
            };
            builder.push(row, definition.id(), value)?;
        }
        self.row_count = self.row_count.saturating_add(1);
        Ok(row)
    }

    /// Seals the aligned arrays into an immutable column store.
    pub fn finish(self) -> Result<ColumnStore, BuildError> {
        let columns = self
            .columns
            .into_iter()
            .map(BuilderColumn::finish)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ColumnStore {
            schema: self.schema,
            columns,
            row_count: self.row_count,
        })
    }

    fn validate_inputs(&self, inputs: &[ColumnInput<'_>]) -> Result<(), BuildError> {
        for (position, input) in inputs.iter().enumerate() {
            if input.column == TIMESTAMP_COLUMN {
                return Err(BuildError::TimestampProvidedAsInput);
            }
            if inputs
                .iter()
                .take(position)
                .any(|candidate| candidate.column == input.column)
            {
                return Err(BuildError::DuplicateColumn(input.column));
            }
            let definition = self
                .schema
                .column(input.column)
                .ok_or(BuildError::UnknownColumn(input.column))?;
            if !input.value.compatible_with(definition.column_type()) {
                return Err(BuildError::TypeMismatch {
                    column: input.column,
                    expected: definition.column_type(),
                    actual: input.value.value_type(),
                });
            }
        }

        for definition in self
            .schema
            .columns()
            .iter()
            .filter(|definition| definition.id() != TIMESTAMP_COLUMN)
        {
            if !definition.is_nullable() && find_input(inputs, definition.id()).is_none() {
                return Err(BuildError::MissingRequiredColumn(definition.id()));
            }
        }
        Ok(())
    }
}

fn find_input<'a, 'value>(
    inputs: &'a [ColumnInput<'value>],
    column: ColumnId,
) -> Option<&'a ColumnInput<'value>> {
    inputs.iter().find(|input| input.column == column)
}
