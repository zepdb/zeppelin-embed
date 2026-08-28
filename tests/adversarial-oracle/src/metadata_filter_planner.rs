//! Independent metadata/filter/planner DTOs, parsers, and exact comparators.
//!
//! This module deliberately uses only the standard library. Production types
//! are translated into these primitives by the adversarial harness.

use std::collections::{BTreeMap, BTreeSet};

pub const I36_CHECKER_ID: &str = "I36.column-roundtrip.v2";
pub const I37_CHECKER_ID: &str = "I37.bitmap-algebra.v2";
pub const I39_CHECKER_ID: &str = "I39.executed-branch.v2";
pub const I38_CHECKER_ID: &str = "I38.pruning-soundness.v2";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScalarCell {
    Null,
    U64(u64),
    I64(i64),
    F64Bits(u64),
    Bool(bool),
    Utf8(Vec<u8>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ColumnKind {
    U64,
    I64,
    F64,
    Bool,
    DictionaryString,
    RawString,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ColumnDefinitionDto {
    pub id: u32,
    pub name: Vec<u8>,
    pub kind: ColumnKind,
    pub nullable: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I36Input {
    pub source: String,
    pub definitions: Vec<ColumnDefinitionDto>,
    pub rows: Vec<BTreeMap<u32, ScalarCell>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I36Observed {
    pub active_rows: Vec<BTreeMap<u32, ScalarCell>>,
    pub raw: ParsedColumns,
    pub reader_rows: Vec<BTreeMap<u32, ScalarCell>>,
    pub public_rows: Vec<BTreeMap<u32, ScalarCell>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ByteSpan {
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StringByteSpans {
    pub full: ByteSpan,
    pub length: ByteSpan,
    pub payload: ByteSpan,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DefinitionByteSpans {
    pub full: ByteSpan,
    pub id: ByteSpan,
    pub kind: ByteSpan,
    pub nullable: ByteSpan,
    pub name: StringByteSpans,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PresenceByteSpans {
    pub length: ByteSpan,
    pub bitmap: ByteSpan,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DictionaryByteSpans {
    pub count: ByteSpan,
    pub entries: Vec<StringByteSpans>,
    pub width: ByteSpan,
    pub reserved: ByteSpan,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PhysicalCell {
    U64(u64),
    I64(i64),
    F64Bits(u64),
    BoolByte(u8),
    DictionaryCode { code: u32, decoded: Option<Vec<u8>> },
    RawUtf8(Vec<u8>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedCell {
    pub present: bool,
    pub logical: ScalarCell,
    pub physical: PhysicalCell,
    pub span: ByteSpan,
    pub length_span: Option<ByteSpan>,
    pub payload_span: ByteSpan,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedColumns {
    pub row_count: u32,
    pub definitions: Vec<ColumnDefinitionDto>,
    pub row_count_span: ByteSpan,
    pub column_count_span: ByteSpan,
    pub definition_spans: BTreeMap<u32, DefinitionByteSpans>,
    pub presence_spans: BTreeMap<u32, PresenceByteSpans>,
    pub dictionary_spans: BTreeMap<u32, DictionaryByteSpans>,
    pub cells: BTreeMap<(u32, u32), ParsedCell>,
}

pub fn compare_i36(input: &I36Input, observed: &I36Observed) -> Result<(), String> {
    if input.definitions != observed.raw.definitions {
        return Err(format!(
            "{I36_CHECKER_ID} source={} schema expected={:?} observed={:?}",
            input.source, input.definitions, observed.raw.definitions
        ));
    }
    let expected_rows = u32::try_from(input.rows.len()).map_err(|_| {
        format!(
            "{I36_CHECKER_ID} source={} row-count overflow",
            input.source
        )
    })?;
    if observed.raw.row_count != expected_rows {
        return Err(format!(
            "{I36_CHECKER_ID} source={} row-count expected={} observed={}",
            input.source, expected_rows, observed.raw.row_count
        ));
    }
    compare_logical_rows(input, "active-public", &observed.active_rows)?;
    compare_logical_rows(input, "sealed-reader", &observed.reader_rows)?;
    compare_logical_rows(input, "reopened-public", &observed.public_rows)?;
    for (row_index, row) in input.rows.iter().enumerate() {
        let row_id = u32::try_from(row_index).map_err(|_| {
            format!(
                "{I36_CHECKER_ID} source={} row-count overflow",
                input.source
            )
        })?;
        for definition in &input.definitions {
            let expected = row.get(&definition.id).cloned().unwrap_or(ScalarCell::Null);
            let cell = observed
                .raw
                .cells
                .get(&(row_id, definition.id))
                .ok_or_else(|| difference(input, definition.id, row_id, &expected, "Missing"))?;
            if cell.logical != expected {
                return Err(difference(
                    input,
                    definition.id,
                    row_id,
                    &expected,
                    &format!("{:?}", cell.logical),
                ));
            }
            let should_be_present = expected != ScalarCell::Null;
            if cell.present != should_be_present {
                return Err(format!(
                    "{I36_CHECKER_ID} source={} column={} row={} expected_present={} observed_present={}",
                    input.source, definition.id, row_id, should_be_present, cell.present
                ));
            }
            validate_physical_cell(input, definition, row_id, &expected, &cell.physical)?;
        }
    }
    Ok(())
}

fn compare_logical_rows(
    input: &I36Input,
    observation: &str,
    observed: &[BTreeMap<u32, ScalarCell>],
) -> Result<(), String> {
    if observed.len() != input.rows.len() {
        return Err(format!(
            "{I36_CHECKER_ID} source={} observation={} row-count expected={} observed={}",
            input.source,
            observation,
            input.rows.len(),
            observed.len()
        ));
    }
    for (row_index, expected_row) in input.rows.iter().enumerate() {
        let observed_row = observed.get(row_index).ok_or_else(|| {
            format!(
                "{I36_CHECKER_ID} source={} observation={} missing-row={row_index}",
                input.source, observation
            )
        })?;
        for definition in &input.definitions {
            let expected = expected_row
                .get(&definition.id)
                .cloned()
                .unwrap_or(ScalarCell::Null);
            let actual = observed_row
                .get(&definition.id)
                .cloned()
                .unwrap_or(ScalarCell::Null);
            if actual != expected {
                return Err(format!(
                    "{I36_CHECKER_ID} source={} column={} row={} expected={expected:?} observed={actual:?} observation={observation}",
                    input.source, definition.id, row_index
                ));
            }
        }
    }
    Ok(())
}

fn validate_physical_cell(
    input: &I36Input,
    definition: &ColumnDefinitionDto,
    row: u32,
    expected: &ScalarCell,
    physical: &PhysicalCell,
) -> Result<(), String> {
    let valid = match (definition.kind, expected, physical) {
        (ColumnKind::U64, ScalarCell::Null, PhysicalCell::U64(0))
        | (ColumnKind::I64, ScalarCell::Null, PhysicalCell::I64(0))
        | (ColumnKind::F64, ScalarCell::Null, PhysicalCell::F64Bits(0))
        | (ColumnKind::Bool, ScalarCell::Null, PhysicalCell::BoolByte(0))
        | (
            ColumnKind::DictionaryString,
            ScalarCell::Null,
            PhysicalCell::DictionaryCode {
                code: 0,
                decoded: _,
            },
        )
        | (ColumnKind::RawString, ScalarCell::Null, PhysicalCell::RawUtf8(_)) => true,
        (ColumnKind::U64, ScalarCell::U64(expected), PhysicalCell::U64(actual)) => {
            expected == actual
        }
        (ColumnKind::I64, ScalarCell::I64(expected), PhysicalCell::I64(actual)) => {
            expected == actual
        }
        (ColumnKind::F64, ScalarCell::F64Bits(expected), PhysicalCell::F64Bits(actual)) => {
            expected == actual
        }
        (ColumnKind::Bool, ScalarCell::Bool(expected), PhysicalCell::BoolByte(actual)) => {
            *actual == u8::from(*expected)
        }
        (
            ColumnKind::DictionaryString,
            ScalarCell::Utf8(expected),
            PhysicalCell::DictionaryCode {
                code: _,
                decoded: Some(actual),
            },
        ) => expected == actual,
        (ColumnKind::RawString, ScalarCell::Utf8(expected), PhysicalCell::RawUtf8(actual)) => {
            expected == actual
        }
        _ => false,
    };
    let canonical_null_raw = !matches!(
        (definition.kind, expected, physical),
        (ColumnKind::RawString, ScalarCell::Null, PhysicalCell::RawUtf8(bytes)) if !bytes.is_empty()
    );
    if valid && canonical_null_raw {
        Ok(())
    } else {
        Err(format!(
            "{I36_CHECKER_ID} source={} column={} row={} expected_physical={expected:?} observed_physical={physical:?}",
            input.source, definition.id, row
        ))
    }
}

fn difference(
    input: &I36Input,
    column: u32,
    row: u32,
    expected: &ScalarCell,
    observed: &str,
) -> String {
    format!(
        "{I36_CHECKER_ID} source={} column={column} row={row} expected={expected:?} observed={observed}",
        input.source
    )
}

pub fn parse_columns(bytes: &[u8]) -> Result<ParsedColumns, String> {
    let mut cursor = CheckedCursor::new("columns", bytes);
    let row_count_start = cursor.position();
    let row_count = cursor.u32()?;
    let row_count_span = ByteSpan {
        start: row_count_start,
        end: cursor.position(),
    };
    let column_count_start = cursor.position();
    let column_count = cursor.usize_from_u32()?;
    let column_count_span = ByteSpan {
        start: column_count_start,
        end: cursor.position(),
    };
    if column_count == 0 {
        return Err("columns required timestamp definition is absent".to_owned());
    }
    let mut definitions = Vec::with_capacity(column_count);
    let mut definition_spans = BTreeMap::new();
    let mut ids = BTreeSet::new();
    let mut names = BTreeSet::new();
    for _ in 0..column_count {
        let definition_start = cursor.position();
        let id_start = cursor.position();
        let id = cursor.u32()?;
        let id_span = ByteSpan {
            start: id_start,
            end: cursor.position(),
        };
        let kind_start = cursor.position();
        let kind = match cursor.u16()? {
            1 => ColumnKind::U64,
            2 => ColumnKind::I64,
            3 => ColumnKind::F64,
            4 => ColumnKind::Bool,
            5 => ColumnKind::DictionaryString,
            6 => ColumnKind::RawString,
            value => return Err(format!("columns unknown column type {value}")),
        };
        let kind_span = ByteSpan {
            start: kind_start,
            end: cursor.position(),
        };
        let nullable_start = cursor.position();
        let nullable = match cursor.u16()? {
            0 => false,
            1 => true,
            value => return Err(format!("columns invalid nullable flag {value}")),
        };
        let nullable_span = ByteSpan {
            start: nullable_start,
            end: cursor.position(),
        };
        let name_length_start = cursor.position();
        let name_length = cursor.usize_from_u32()?;
        let name_length_span = ByteSpan {
            start: name_length_start,
            end: cursor.position(),
        };
        let name_payload_start = cursor.position();
        let name = cursor.take(name_length)?.to_vec();
        let name_payload_span = ByteSpan {
            start: name_payload_start,
            end: cursor.position(),
        };
        std::str::from_utf8(&name).map_err(|error| format!("column name UTF-8: {error}"))?;
        if !ids.insert(id) {
            return Err(format!("columns duplicate column id {id}"));
        }
        if !names.insert(name.clone()) {
            return Err(format!(
                "columns duplicate column name {}",
                String::from_utf8_lossy(&name)
            ));
        }
        definitions.push(ColumnDefinitionDto {
            id,
            name,
            kind,
            nullable,
        });
        definition_spans.insert(
            id,
            DefinitionByteSpans {
                full: ByteSpan {
                    start: definition_start,
                    end: cursor.position(),
                },
                id: id_span,
                kind: kind_span,
                nullable: nullable_span,
                name: StringByteSpans {
                    full: ByteSpan {
                        start: name_length_start,
                        end: cursor.position(),
                    },
                    length: name_length_span,
                    payload: name_payload_span,
                },
            },
        );
    }
    let timestamp = definitions
        .first()
        .ok_or_else(|| "columns required timestamp definition is absent".to_owned())?;
    if timestamp.id != 0
        || timestamp.name.as_slice() != b"ts"
        || timestamp.kind != ColumnKind::I64
        || timestamp.nullable
    {
        return Err("columns timestamp definition is not canonical".to_owned());
    }
    if definitions
        .iter()
        .skip(1)
        .any(|definition| definition.id == 0 || definition.name.as_slice() == b"ts")
    {
        return Err("columns reserved timestamp definition is duplicated".to_owned());
    }

    let mut cells = BTreeMap::new();
    let mut presence_spans = BTreeMap::new();
    let mut dictionary_spans = BTreeMap::new();
    for definition in &definitions {
        let presence_length_start = cursor.position();
        let presence_length = cursor.usize_from_u32()?;
        let presence_length_span = ByteSpan {
            start: presence_length_start,
            end: cursor.position(),
        };
        let expected_presence = (row_count as usize).div_ceil(8);
        if presence_length != expected_presence {
            return Err(format!(
                "column {} presence length {presence_length}, expected {expected_presence}",
                definition.id
            ));
        }
        let presence_start = cursor.position();
        let presence = cursor.take(presence_length)?.to_vec();
        presence_spans.insert(
            definition.id,
            PresenceByteSpans {
                length: presence_length_span,
                bitmap: ByteSpan {
                    start: presence_start,
                    end: cursor.position(),
                },
            },
        );
        validate_presence_tail(row_count, &presence)?;
        parse_column_cells(
            definition,
            row_count,
            &presence,
            &mut cursor,
            &mut cells,
            &mut dictionary_spans,
        )?;
    }
    cursor.finish()?;
    Ok(ParsedColumns {
        row_count,
        definitions,
        row_count_span,
        column_count_span,
        definition_spans,
        presence_spans,
        dictionary_spans,
        cells,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedAlive {
    pub row_count: u32,
    pub live: BTreeSet<u32>,
    pub bitmap_span: ByteSpan,
}

pub fn parse_alive(bytes: &[u8]) -> Result<ParsedAlive, String> {
    let mut cursor = CheckedCursor::new("alive", bytes);
    let row_count = cursor.u32()?;
    let byte_length = cursor.usize_from_u32()?;
    let expected = (row_count as usize).div_ceil(8);
    if byte_length != expected {
        return Err(format!(
            "alive bitmap length {byte_length}, expected {expected} for {row_count} rows"
        ));
    }
    let start = cursor.position();
    let bitmap = cursor.take(byte_length)?.to_vec();
    let bitmap_span = ByteSpan {
        start,
        end: cursor.position(),
    };
    validate_presence_tail(row_count, &bitmap)
        .map_err(|_| "alive non-zero bitmap tail padding".to_owned())?;
    cursor.finish()?;
    let live = (0..row_count)
        .filter(|row| presence_contains(&bitmap, *row))
        .collect();
    Ok(ParsedAlive {
        row_count,
        live,
        bitmap_span,
    })
}

fn parse_column_cells(
    definition: &ColumnDefinitionDto,
    row_count: u32,
    presence: &[u8],
    cursor: &mut CheckedCursor<'_>,
    cells: &mut BTreeMap<(u32, u32), ParsedCell>,
    dictionary_spans: &mut BTreeMap<u32, DictionaryByteSpans>,
) -> Result<(), String> {
    match definition.kind {
        ColumnKind::DictionaryString => {
            let dictionary_count_start = cursor.position();
            let dictionary_length = cursor.usize_from_u32()?;
            let dictionary_count_span = ByteSpan {
                start: dictionary_count_start,
                end: cursor.position(),
            };
            let mut dictionary = Vec::with_capacity(dictionary_length);
            let mut entries = Vec::with_capacity(dictionary_length);
            for _ in 0..dictionary_length {
                let (bytes, spans) = cursor.string_bytes()?;
                dictionary.push(bytes);
                entries.push(spans);
            }
            let width_start = cursor.position();
            let width = cursor.u16()?;
            let width_span = ByteSpan {
                start: width_start,
                end: cursor.position(),
            };
            let reserved_start = cursor.position();
            let reserved = cursor.u16()?;
            let reserved_span = ByteSpan {
                start: reserved_start,
                end: cursor.position(),
            };
            if reserved != 0 {
                return Err("dictionary reserved field is non-zero".to_owned());
            }
            if !matches!(width, 2 | 4) {
                return Err(format!("invalid dictionary width {width}"));
            }
            dictionary_spans.insert(
                definition.id,
                DictionaryByteSpans {
                    count: dictionary_count_span,
                    entries,
                    width: width_span,
                    reserved: reserved_span,
                },
            );
            for row in 0..row_count {
                let start = cursor.position();
                let code = if width == 2 {
                    u32::from(cursor.u16()?)
                } else {
                    cursor.u32()?
                };
                let span = ByteSpan {
                    start,
                    end: cursor.position(),
                };
                let present = presence_contains(presence, row);
                let decoded = usize::try_from(code)
                    .ok()
                    .and_then(|position| dictionary.get(position).cloned());
                if present && decoded.is_none() {
                    return Err(format!("dictionary code {code} out of range"));
                }
                if !present && code != 0 {
                    return Err(format!(
                        "column {} null row {row} has noncanonical dictionary code {code}",
                        definition.id
                    ));
                }
                let logical = decoded
                    .as_ref()
                    .filter(|_| present)
                    .cloned()
                    .map(ScalarCell::Utf8)
                    .unwrap_or(ScalarCell::Null);
                cells.insert(
                    (row, definition.id),
                    ParsedCell {
                        present,
                        logical,
                        physical: PhysicalCell::DictionaryCode { code, decoded },
                        span,
                        length_span: None,
                        payload_span: span,
                    },
                );
            }
        }
        ColumnKind::RawString => {
            for row in 0..row_count {
                let (bytes, spans) = cursor.string_bytes()?;
                let present = presence_contains(presence, row);
                if !present && !bytes.is_empty() {
                    return Err(format!(
                        "column {} null row {row} has noncanonical raw string bytes",
                        definition.id
                    ));
                }
                let logical = if present {
                    ScalarCell::Utf8(bytes.clone())
                } else {
                    ScalarCell::Null
                };
                cells.insert(
                    (row, definition.id),
                    ParsedCell {
                        present,
                        logical,
                        physical: PhysicalCell::RawUtf8(bytes),
                        span: spans.full,
                        length_span: Some(spans.length),
                        payload_span: spans.payload,
                    },
                );
            }
        }
        _ => {
            for row in 0..row_count {
                let start = cursor.position();
                let physical = match definition.kind {
                    ColumnKind::U64 => PhysicalCell::U64(cursor.u64()?),
                    ColumnKind::I64 => PhysicalCell::I64(cursor.i64()?),
                    ColumnKind::F64 => PhysicalCell::F64Bits(cursor.u64()?),
                    ColumnKind::Bool => {
                        let value = cursor.u8()?;
                        if value > 1 {
                            return Err(format!("invalid Boolean byte {value}"));
                        }
                        PhysicalCell::BoolByte(value)
                    }
                    ColumnKind::DictionaryString | ColumnKind::RawString => {
                        return Err("internal column parser dispatch mismatch".to_owned());
                    }
                };
                let span = ByteSpan {
                    start,
                    end: cursor.position(),
                };
                let present = presence_contains(presence, row);
                let logical = if present {
                    match physical {
                        PhysicalCell::U64(value) => ScalarCell::U64(value),
                        PhysicalCell::I64(value) => ScalarCell::I64(value),
                        PhysicalCell::F64Bits(value) => ScalarCell::F64Bits(value),
                        PhysicalCell::BoolByte(value) => ScalarCell::Bool(value != 0),
                        PhysicalCell::DictionaryCode { .. } | PhysicalCell::RawUtf8(_) => {
                            return Err("internal physical cell mismatch".to_owned());
                        }
                    }
                } else {
                    ScalarCell::Null
                };
                let null_is_canonical = match physical {
                    PhysicalCell::U64(value) => value == 0,
                    PhysicalCell::I64(value) => value == 0,
                    PhysicalCell::F64Bits(value) => value == 0,
                    PhysicalCell::BoolByte(value) => value == 0,
                    PhysicalCell::DictionaryCode { .. } | PhysicalCell::RawUtf8(_) => false,
                };
                if !present && !null_is_canonical {
                    return Err(format!(
                        "column {} null row {row} has noncanonical placeholder {physical:?}",
                        definition.id
                    ));
                }
                cells.insert(
                    (row, definition.id),
                    ParsedCell {
                        present,
                        logical,
                        physical,
                        span,
                        length_span: None,
                        payload_span: span,
                    },
                );
            }
        }
    }
    Ok(())
}

fn validate_presence_tail(row_count: u32, bytes: &[u8]) -> Result<(), String> {
    if !row_count.is_multiple_of(8) {
        let used = row_count % 8;
        if bytes
            .last()
            .is_some_and(|last| last & !((1_u8 << used) - 1) != 0)
        {
            return Err("non-zero presence tail padding".to_owned());
        }
    }
    Ok(())
}

fn presence_contains(bytes: &[u8], row: u32) -> bool {
    let position = row as usize;
    bytes
        .get(position / 8)
        .is_some_and(|byte| byte & (1_u8 << (position % 8)) != 0)
}

struct CheckedCursor<'a> {
    artifact: &'static str,
    bytes: &'a [u8],
    position: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetadataRowDto {
    pub row_id: u32,
    pub cells: BTreeMap<u32, ScalarCell>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RangeBoundDto {
    pub value: ScalarCell,
    pub inclusive: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PredicateDto {
    Eq {
        column: u32,
        value: ScalarCell,
    },
    In {
        column: u32,
        values: Vec<ScalarCell>,
    },
    Range {
        column: u32,
        lower: Option<RangeBoundDto>,
        upper: Option<RangeBoundDto>,
    },
    Exists(u32),
    IsNull(u32),
    And(Vec<PredicateDto>),
    Or(Vec<PredicateDto>),
    Not(Box<PredicateDto>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I37Input {
    pub rows: Vec<MetadataRowDto>,
    pub live: BTreeSet<u32>,
    pub predicate: PredicateDto,
    pub sources: Vec<I37SourceInputDto>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I37Observed {
    pub evaluator: BTreeSet<u32>,
    pub public_results: Vec<u32>,
    pub sources: Vec<I37SourceObservedDto>,
    pub allow_list_threshold: u64,
}

/// Stable primitive mismatch category retained independently of display text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DifferenceKind {
    ExtraRow,
    MissingRow,
    DuplicateRow,
    DeadRow,
    PrimitiveMismatch,
    UnsoundPrune,
    ReportReceiptMismatch,
    ContractMismatch,
}

/// The first exact primitive disagreement emitted by a family checker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FirstDifference {
    pub checker_id: &'static str,
    pub path: String,
    pub kind: DifferenceKind,
    pub row: Option<u32>,
    pub expected: String,
    pub observed: String,
}

/// Canonical input/observation identity plus a structured first difference.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OracleAttestation {
    pub checker_id: &'static str,
    pub canonical_version: u32,
    pub input_digest: u64,
    pub observed_digest: u64,
    pub input_bytes: Vec<u8>,
    pub observed_bytes: Vec<u8>,
    pub first_difference: Option<FirstDifference>,
}

pub const METADATA_CANONICAL_VERSION: u32 = 1;

#[must_use]
pub fn canonical_digest(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[must_use]
pub fn attest_i37(input: &I37Input, observed: &I37Observed) -> OracleAttestation {
    let input_bytes = canonical_i37_input_bytes(input);
    let observed_bytes = canonical_i37_observed_bytes(observed);
    OracleAttestation {
        checker_id: I37_CHECKER_ID,
        canonical_version: METADATA_CANONICAL_VERSION,
        input_digest: canonical_digest(&input_bytes),
        observed_digest: canonical_digest(&observed_bytes),
        input_bytes,
        observed_bytes,
        first_difference: first_i37_difference(input, observed),
    }
}

#[must_use]
pub fn canonical_i37_input_digest(input: &I37Input) -> u64 {
    canonical_digest(&canonical_i37_input_bytes(input))
}

#[must_use]
pub fn canonical_i37_input_bytes(input: &I37Input) -> Vec<u8> {
    let mut canonical = CanonicalBytes::new(b"metadata/I37/input/v1");
    canonical.rows(&input.rows);
    canonical.set(&input.live);
    canonical.predicate(&input.predicate);
    canonical.len(input.sources.len());
    for source in &input.sources {
        canonical.string(&source.source);
        canonical.boolean(source.sealed);
        canonical.rows(&source.rows);
        canonical.set(&source.live);
    }
    canonical.into_bytes()
}

#[must_use]
pub fn canonical_i37_observed_digest(observed: &I37Observed) -> u64 {
    canonical_digest(&canonical_i37_observed_bytes(observed))
}

#[must_use]
pub fn canonical_i37_observed_bytes(observed: &I37Observed) -> Vec<u8> {
    let mut canonical = CanonicalBytes::new(b"metadata/I37/observed/v1");
    canonical.set(&observed.evaluator);
    canonical.u32_slice(&observed.public_results);
    canonical.len(observed.sources.len());
    for source in &observed.sources {
        canonical.string(&source.source);
        canonical.boolean(source.sealed);
        canonical.u32(source.row_count);
        canonical.set(&source.live);
        canonical.set(&source.evaluator);
        canonical.u32_slice(&source.public_results);
        canonical.report(&source.report);
        canonical.receipt(&source.receipt);
    }
    canonical.u64(observed.allow_list_threshold);
    canonical.into_bytes()
}

#[must_use]
pub fn attest_i36(input: &I36Input, observed: &I36Observed) -> OracleAttestation {
    let input_bytes = canonical_i36_input_bytes(input);
    let observed_bytes = canonical_i36_observed_bytes(observed);
    OracleAttestation {
        checker_id: I36_CHECKER_ID,
        canonical_version: METADATA_CANONICAL_VERSION,
        input_digest: canonical_digest(&input_bytes),
        observed_digest: canonical_digest(&observed_bytes),
        input_bytes,
        observed_bytes,
        first_difference: first_i36_difference(input, observed),
    }
}

#[must_use]
pub fn canonical_i36_input_digest(input: &I36Input) -> u64 {
    canonical_digest(&canonical_i36_input_bytes(input))
}

#[must_use]
pub fn canonical_i36_input_bytes(input: &I36Input) -> Vec<u8> {
    let mut canonical = CanonicalBytes::new(b"metadata/I36/input/v1");
    canonical.string(&input.source);
    canonical.definitions(&input.definitions);
    canonical.logical_rows(&input.rows);
    canonical.into_bytes()
}

#[must_use]
pub fn canonical_i36_observed_digest(observed: &I36Observed) -> u64 {
    canonical_digest(&canonical_i36_observed_bytes(observed))
}

#[must_use]
pub fn canonical_i36_observed_bytes(observed: &I36Observed) -> Vec<u8> {
    let mut canonical = CanonicalBytes::new(b"metadata/I36/observed/v1");
    canonical.logical_rows(&observed.active_rows);
    canonical.parsed_columns(&observed.raw);
    canonical.logical_rows(&observed.reader_rows);
    canonical.logical_rows(&observed.public_rows);
    canonical.into_bytes()
}

#[must_use]
pub fn attest_i38(input: &I38Input, observed: &I38Observed) -> OracleAttestation {
    let input_bytes = canonical_i38_input_bytes(input);
    let observed_bytes = canonical_i38_observed_bytes(observed);
    OracleAttestation {
        checker_id: I38_CHECKER_ID,
        canonical_version: METADATA_CANONICAL_VERSION,
        input_digest: canonical_digest(&input_bytes),
        observed_digest: canonical_digest(&observed_bytes),
        input_bytes,
        observed_bytes,
        first_difference: first_i38_difference(input, observed),
    }
}

#[must_use]
pub fn canonical_i38_input_digest(input: &I38Input) -> u64 {
    canonical_digest(&canonical_i38_input_bytes(input))
}

#[must_use]
pub fn canonical_i38_input_bytes(input: &I38Input) -> Vec<u8> {
    let mut canonical = CanonicalBytes::new(b"metadata/I38/input/v1");
    canonical.len(input.sources.len());
    for source in &input.sources {
        canonical.string(&source.source);
        canonical.boolean(source.sealed);
        canonical.source_range(source.range);
    }
    canonical.len(input.rows.len());
    for row in &input.rows {
        canonical.string(&row.source);
        canonical.u32(row.row_id);
        canonical.u128(row.document_id);
        canonical.cells(&row.cells);
    }
    canonical.len(input.live.len());
    for (source, row) in &input.live {
        canonical.string(source);
        canonical.u32(*row);
    }
    canonical.predicate(&input.predicate);
    canonical.exact_hits(&input.unfiltered_exact);
    canonical.u64(input.expected_delete_records);
    canonical.into_bytes()
}

#[must_use]
pub fn canonical_i38_observed_digest(observed: &I38Observed) -> u64 {
    canonical_digest(&canonical_i38_observed_bytes(observed))
}

#[must_use]
pub fn canonical_i38_observed_bytes(observed: &I38Observed) -> Vec<u8> {
    let mut canonical = CanonicalBytes::new(b"metadata/I38/observed/v1");
    canonical.exact_hits(&observed.filtered_exact);
    canonical.len(observed.pruned_sources.len());
    for source in &observed.pruned_sources {
        canonical.string(source);
    }
    canonical.reports(&observed.reports);
    canonical.receipts(&observed.execution_receipts);
    canonical.u64(observed.allow_list_threshold);
    canonical.u64(observed.wal_delete_records);
    canonical.into_bytes()
}

#[must_use]
pub fn attest_i39(expected: &[I39ExpectedCase], observed: &I39Observed) -> OracleAttestation {
    let input_bytes = canonical_i39_input_bytes(expected);
    let observed_bytes = canonical_i39_observed_bytes(observed);
    OracleAttestation {
        checker_id: I39_CHECKER_ID,
        canonical_version: METADATA_CANONICAL_VERSION,
        input_digest: canonical_digest(&input_bytes),
        observed_digest: canonical_digest(&observed_bytes),
        input_bytes,
        observed_bytes,
        first_difference: first_i39_difference(expected, observed),
    }
}

#[must_use]
pub fn canonical_i39_input_digest(expected: &[I39ExpectedCase]) -> u64 {
    canonical_digest(&canonical_i39_input_bytes(expected))
}

#[must_use]
pub fn canonical_i39_input_bytes(expected: &[I39ExpectedCase]) -> Vec<u8> {
    let mut canonical = CanonicalBytes::new(b"metadata/I39/input/v1");
    canonical.len(expected.len());
    for case in expected {
        canonical.i39_case(case);
    }
    canonical.into_bytes()
}

#[must_use]
pub fn canonical_i39_observed_digest(observed: &I39Observed) -> u64 {
    canonical_digest(&canonical_i39_observed_bytes(observed))
}

#[must_use]
pub fn canonical_i39_observed_bytes(observed: &I39Observed) -> Vec<u8> {
    let mut canonical = CanonicalBytes::new(b"metadata/I39/observed/v1");
    canonical.reports(&observed.reports);
    canonical.reports(&observed.diagnostics_reports);
    canonical.receipts(&observed.receipts);
    canonical.u64(observed.allow_list_threshold);
    canonical.into_bytes()
}

fn first_i36_difference(input: &I36Input, observed: &I36Observed) -> Option<FirstDifference> {
    if input.definitions != observed.raw.definitions {
        return Some(FirstDifference {
            checker_id: I36_CHECKER_ID,
            path: format!("source={}/schema", input.source),
            kind: DifferenceKind::PrimitiveMismatch,
            row: None,
            expected: format!("{:?}", input.definitions),
            observed: format!("{:?}", observed.raw.definitions),
        });
    }
    if u32::try_from(input.rows.len()).ok() != Some(observed.raw.row_count) {
        return Some(FirstDifference {
            checker_id: I36_CHECKER_ID,
            path: format!("source={}/row-count", input.source),
            kind: DifferenceKind::PrimitiveMismatch,
            row: None,
            expected: input.rows.len().to_string(),
            observed: observed.raw.row_count.to_string(),
        });
    }
    for (phase, rows) in [
        ("active-public", observed.active_rows.as_slice()),
        ("sealed-reader", observed.reader_rows.as_slice()),
        ("reopened-public", observed.public_rows.as_slice()),
    ] {
        if rows.len() != input.rows.len() {
            return Some(FirstDifference {
                checker_id: I36_CHECKER_ID,
                path: format!("source={}/{phase}/row-count", input.source),
                kind: DifferenceKind::PrimitiveMismatch,
                row: None,
                expected: input.rows.len().to_string(),
                observed: rows.len().to_string(),
            });
        }
        for (row_index, expected_row) in input.rows.iter().enumerate() {
            let Ok(row) = u32::try_from(row_index) else {
                break;
            };
            for definition in &input.definitions {
                let expected = expected_row
                    .get(&definition.id)
                    .cloned()
                    .unwrap_or(ScalarCell::Null);
                let actual = rows[row_index]
                    .get(&definition.id)
                    .cloned()
                    .unwrap_or(ScalarCell::Null);
                if actual != expected {
                    return Some(FirstDifference {
                        checker_id: I36_CHECKER_ID,
                        path: format!(
                            "source={}/column={}/row={row}/{phase}",
                            input.source, definition.id
                        ),
                        kind: DifferenceKind::PrimitiveMismatch,
                        row: Some(row),
                        expected: format!("{expected:?}"),
                        observed: format!("{actual:?}"),
                    });
                }
            }
        }
    }
    for (row_index, expected_row) in input.rows.iter().enumerate() {
        let Ok(row) = u32::try_from(row_index) else {
            break;
        };
        for definition in &input.definitions {
            let expected = expected_row
                .get(&definition.id)
                .cloned()
                .unwrap_or(ScalarCell::Null);
            let Some(cell) = observed.raw.cells.get(&(row, definition.id)) else {
                return Some(FirstDifference {
                    checker_id: I36_CHECKER_ID,
                    path: format!(
                        "source={}/column={}/row={row}/raw.logical",
                        input.source, definition.id
                    ),
                    kind: DifferenceKind::PrimitiveMismatch,
                    row: Some(row),
                    expected: format!("{expected:?}"),
                    observed: "missing".to_owned(),
                });
            };
            if cell.logical != expected {
                return Some(FirstDifference {
                    checker_id: I36_CHECKER_ID,
                    path: format!(
                        "source={}/column={}/row={row}/raw.logical",
                        input.source, definition.id
                    ),
                    kind: DifferenceKind::PrimitiveMismatch,
                    row: Some(row),
                    expected: format!("{expected:?}"),
                    observed: format!("{:?}", cell.logical),
                });
            }
        }
    }
    compare_i36(input, observed)
        .err()
        .map(|error| contract_difference(I36_CHECKER_ID, "column-contract", error))
}

fn first_i38_difference(input: &I38Input, observed: &I38Observed) -> Option<FirstDifference> {
    let expected = match expected_i38(input) {
        Ok(expected) => expected,
        Err(error) => {
            return Some(contract_difference(I38_CHECKER_ID, "input", error));
        }
    };
    for source in &observed.pruned_sources {
        if let Some(row) = input.rows.iter().find(|row| {
            row.source == *source
                && input.live.contains(&(row.source.clone(), row.row_id))
                && matches_predicate_row(&input.predicate, &row.cells)
        }) {
            let retained = expected
                .iter()
                .find(|hit| hit.source == row.source && hit.row_id == row.row_id)
                .map_or_else(
                    || "retained matching row".to_owned(),
                    |hit| {
                        format!(
                            "retained document={} distance_bits={}",
                            hit.document_id, hit.distance_bits
                        )
                    },
                );
            return Some(FirstDifference {
                checker_id: I38_CHECKER_ID,
                path: format!("source={source}/row={}/prune-decision", row.row_id),
                kind: DifferenceKind::UnsoundPrune,
                row: Some(row.row_id),
                expected: retained,
                observed: "pruned".to_owned(),
            });
        }
    }
    compare_i38(input, observed)
        .err()
        .map(|error| contract_difference(I38_CHECKER_ID, "pruning-contract", error))
}

fn first_i39_difference(
    expected: &[I39ExpectedCase],
    observed: &I39Observed,
) -> Option<FirstDifference> {
    for report in &observed.reports {
        if let Some(receipt) = observed
            .receipts
            .iter()
            .find(|receipt| receipt.key == report.key)
            && report.branch != receipt.branch
        {
            return Some(FirstDifference {
                checker_id: I39_CHECKER_ID,
                path: format!(
                    "query={}/source={}/branch",
                    report.key.query_id, report.key.source
                ),
                kind: DifferenceKind::ReportReceiptMismatch,
                row: None,
                expected: format!("reported={:?}", report.branch),
                observed: format!("executed={:?}", receipt.branch),
            });
        }
    }
    compare_i39_expected(expected, observed)
        .err()
        .map(|error| contract_difference(I39_CHECKER_ID, "execution-contract", error))
}

fn contract_difference(checker_id: &'static str, path: &str, error: String) -> FirstDifference {
    FirstDifference {
        checker_id,
        path: path.to_owned(),
        kind: DifferenceKind::ContractMismatch,
        row: None,
        expected: "exact primitive contract".to_owned(),
        observed: error,
    }
}

fn first_i37_difference(input: &I37Input, observed: &I37Observed) -> Option<FirstDifference> {
    let expected = match evaluate_i37(input) {
        Ok(expected) => expected,
        Err(error) => {
            return Some(FirstDifference {
                checker_id: I37_CHECKER_ID,
                path: "input".to_owned(),
                kind: DifferenceKind::ContractMismatch,
                row: None,
                expected: "valid primitive input".to_owned(),
                observed: error,
            });
        }
    };
    let path = predicate_path("root", &input.predicate);
    if let Some(row) = observed.evaluator.difference(&expected.result).next() {
        return Some(row_difference(&path, DifferenceKind::ExtraRow, *row));
    }
    if let Some(row) = expected.result.difference(&observed.evaluator).next() {
        return Some(row_difference(&path, DifferenceKind::MissingRow, *row));
    }
    let mut public = BTreeSet::new();
    for row in &observed.public_results {
        if !public.insert(*row) {
            return Some(FirstDifference {
                checker_id: I37_CHECKER_ID,
                path,
                kind: DifferenceKind::DuplicateRow,
                row: Some(*row),
                expected: "one occurrence".to_owned(),
                observed: "multiple occurrences".to_owned(),
            });
        }
    }
    if let Some(row) = public.iter().find(|row| !input.live.contains(row)) {
        return Some(FirstDifference {
            checker_id: I37_CHECKER_ID,
            path,
            kind: DifferenceKind::DeadRow,
            row: Some(*row),
            expected: "absent from alive-bounded result".to_owned(),
            observed: "present".to_owned(),
        });
    }
    if let Some(row) = public.difference(&expected.result).next() {
        return Some(row_difference(&path, DifferenceKind::ExtraRow, *row));
    }
    if let Some(row) = expected.result.difference(&public).next() {
        return Some(row_difference(&path, DifferenceKind::MissingRow, *row));
    }
    compare_i37(input, observed)
        .err()
        .map(|error| FirstDifference {
            checker_id: I37_CHECKER_ID,
            path: "source-ledger".to_owned(),
            kind: DifferenceKind::ContractMismatch,
            row: None,
            expected: "exact source/report/receipt contract".to_owned(),
            observed: error,
        })
}

fn row_difference(path: &str, kind: DifferenceKind, row: u32) -> FirstDifference {
    let (expected, observed) = match kind {
        DifferenceKind::ExtraRow => ("absent", "present"),
        DifferenceKind::MissingRow => ("present", "absent"),
        DifferenceKind::DuplicateRow
        | DifferenceKind::DeadRow
        | DifferenceKind::PrimitiveMismatch
        | DifferenceKind::UnsoundPrune
        | DifferenceKind::ReportReceiptMismatch
        | DifferenceKind::ContractMismatch => ("contract", "mismatch"),
    };
    FirstDifference {
        checker_id: I37_CHECKER_ID,
        path: path.to_owned(),
        kind,
        row: Some(row),
        expected: expected.to_owned(),
        observed: observed.to_owned(),
    }
}

struct CanonicalBytes {
    bytes: Vec<u8>,
}

impl CanonicalBytes {
    fn new(domain: &[u8]) -> Self {
        let mut canonical = Self { bytes: Vec::new() };
        canonical.blob(domain);
        canonical
    }

    fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    fn tag(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn boolean(&mut self, value: bool) {
        self.tag(u8::from(value));
    }

    fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn i64(&mut self, value: i64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn len(&mut self, value: usize) {
        self.u64(match u64::try_from(value) {
            Ok(value) => value,
            Err(error) => panic!("metadata canonical length exceeds u64: {error}"),
        });
    }

    fn blob(&mut self, value: &[u8]) {
        self.len(value.len());
        self.bytes.extend_from_slice(value);
    }

    fn string(&mut self, value: &str) {
        self.blob(value.as_bytes());
    }

    fn scalar(&mut self, value: &ScalarCell) {
        match value {
            ScalarCell::Null => self.tag(0),
            ScalarCell::U64(value) => {
                self.tag(1);
                self.u64(*value);
            }
            ScalarCell::I64(value) => {
                self.tag(2);
                self.i64(*value);
            }
            ScalarCell::F64Bits(value) => {
                self.tag(3);
                self.u64(*value);
            }
            ScalarCell::Bool(value) => {
                self.tag(4);
                self.boolean(*value);
            }
            ScalarCell::Utf8(value) => {
                self.tag(5);
                self.blob(value);
            }
        }
    }

    fn row(&mut self, row: &MetadataRowDto) {
        self.u32(row.row_id);
        self.len(row.cells.len());
        for (column, cell) in &row.cells {
            self.u32(*column);
            self.scalar(cell);
        }
    }

    fn rows(&mut self, rows: &[MetadataRowDto]) {
        self.len(rows.len());
        for row in rows {
            self.row(row);
        }
    }

    fn set(&mut self, rows: &BTreeSet<u32>) {
        self.len(rows.len());
        for row in rows {
            self.u32(*row);
        }
    }

    fn u32_slice(&mut self, rows: &[u32]) {
        self.len(rows.len());
        for row in rows {
            self.u32(*row);
        }
    }

    fn bound(&mut self, bound: &Option<RangeBoundDto>) {
        match bound {
            None => self.tag(0),
            Some(bound) => {
                self.tag(1);
                self.scalar(&bound.value);
                self.boolean(bound.inclusive);
            }
        }
    }

    fn predicate(&mut self, predicate: &PredicateDto) {
        match predicate {
            PredicateDto::Eq { column, value } => {
                self.tag(0);
                self.u32(*column);
                self.scalar(value);
            }
            PredicateDto::In { column, values } => {
                self.tag(1);
                self.u32(*column);
                self.len(values.len());
                for value in values {
                    self.scalar(value);
                }
            }
            PredicateDto::Range {
                column,
                lower,
                upper,
            } => {
                self.tag(2);
                self.u32(*column);
                self.bound(lower);
                self.bound(upper);
            }
            PredicateDto::Exists(column) => {
                self.tag(3);
                self.u32(*column);
            }
            PredicateDto::IsNull(column) => {
                self.tag(4);
                self.u32(*column);
            }
            PredicateDto::And(children) => {
                self.tag(5);
                self.len(children.len());
                for child in children {
                    self.predicate(child);
                }
            }
            PredicateDto::Or(children) => {
                self.tag(6);
                self.len(children.len());
                for child in children {
                    self.predicate(child);
                }
            }
            PredicateDto::Not(child) => {
                self.tag(7);
                self.predicate(child);
            }
        }
    }

    fn branch(&mut self, branch: ExecutionBranchDto) {
        self.tag(match branch {
            ExecutionBranchDto::Pruned => 0,
            ExecutionBranchDto::ExactAllowList => 1,
            ExecutionBranchDto::MaskedScan => 2,
            ExecutionBranchDto::FilteredGraph => 3,
            ExecutionBranchDto::GraphExactFallback => 4,
        });
    }

    fn fallback(&mut self, fallback: FallbackReasonDto) {
        self.tag(match fallback {
            FallbackReasonDto::None => 0,
            FallbackReasonDto::VisitedBudget => 1,
            FallbackReasonDto::CandidateShortfall => 2,
            FallbackReasonDto::EfWidened => 3,
        });
    }

    fn key(&mut self, key: &QuerySourceKey) {
        self.u64(key.query_id);
        self.string(&key.source);
    }

    fn report(&mut self, report: &BranchReportDto) {
        self.key(&report.key);
        self.branch(report.branch);
        self.fallback(report.fallback);
        self.u64(report.filter_cardinality);
    }

    fn optional_u64(&mut self, value: Option<u64>) {
        match value {
            None => self.tag(0),
            Some(value) => {
                self.tag(1);
                self.u64(value);
            }
        }
    }

    fn receipt(&mut self, receipt: &ExecutionReceiptDto) {
        self.key(&receipt.key);
        self.branch(receipt.branch);
        self.fallback(receipt.fallback);
        self.u64(receipt.row_count);
        self.u64(receipt.filter_cardinality);
        self.u64(receipt.rows_examined);
        self.u64(receipt.allowed_rows_examined);
        self.u64(receipt.vectors_scored);
        self.u64(receipt.graph_nodes_visited);
        self.u64(receipt.exact_fallback_rows_examined);
        self.u64(receipt.returned_candidates);
        self.optional_u64(receipt.ef_effective);
        self.optional_u64(receipt.visited_budget);
        self.boolean(receipt.sealed);
    }

    fn u128(&mut self, value: u128) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn usize_value(&mut self, value: usize) {
        self.len(value);
    }

    fn column_kind(&mut self, kind: ColumnKind) {
        self.tag(match kind {
            ColumnKind::U64 => 0,
            ColumnKind::I64 => 1,
            ColumnKind::F64 => 2,
            ColumnKind::Bool => 3,
            ColumnKind::DictionaryString => 4,
            ColumnKind::RawString => 5,
        });
    }

    fn definition(&mut self, definition: &ColumnDefinitionDto) {
        self.u32(definition.id);
        self.blob(&definition.name);
        self.column_kind(definition.kind);
        self.boolean(definition.nullable);
    }

    fn definitions(&mut self, definitions: &[ColumnDefinitionDto]) {
        self.len(definitions.len());
        for definition in definitions {
            self.definition(definition);
        }
    }

    fn cells(&mut self, cells: &BTreeMap<u32, ScalarCell>) {
        self.len(cells.len());
        for (column, cell) in cells {
            self.u32(*column);
            self.scalar(cell);
        }
    }

    fn logical_rows(&mut self, rows: &[BTreeMap<u32, ScalarCell>]) {
        self.len(rows.len());
        for row in rows {
            self.cells(row);
        }
    }

    fn span(&mut self, span: ByteSpan) {
        self.usize_value(span.start);
        self.usize_value(span.end);
    }

    fn string_spans(&mut self, spans: StringByteSpans) {
        self.span(spans.full);
        self.span(spans.length);
        self.span(spans.payload);
    }

    fn physical(&mut self, physical: &PhysicalCell) {
        match physical {
            PhysicalCell::U64(value) => {
                self.tag(0);
                self.u64(*value);
            }
            PhysicalCell::I64(value) => {
                self.tag(1);
                self.i64(*value);
            }
            PhysicalCell::F64Bits(value) => {
                self.tag(2);
                self.u64(*value);
            }
            PhysicalCell::BoolByte(value) => {
                self.tag(3);
                self.tag(*value);
            }
            PhysicalCell::DictionaryCode { code, decoded } => {
                self.tag(4);
                self.u32(*code);
                match decoded {
                    None => self.tag(0),
                    Some(decoded) => {
                        self.tag(1);
                        self.blob(decoded);
                    }
                }
            }
            PhysicalCell::RawUtf8(value) => {
                self.tag(5);
                self.blob(value);
            }
        }
    }

    fn parsed_columns(&mut self, columns: &ParsedColumns) {
        self.u32(columns.row_count);
        self.definitions(&columns.definitions);
        self.span(columns.row_count_span);
        self.span(columns.column_count_span);
        self.len(columns.definition_spans.len());
        for (column, spans) in &columns.definition_spans {
            self.u32(*column);
            self.span(spans.full);
            self.span(spans.id);
            self.span(spans.kind);
            self.span(spans.nullable);
            self.string_spans(spans.name);
        }
        self.len(columns.presence_spans.len());
        for (column, spans) in &columns.presence_spans {
            self.u32(*column);
            self.span(spans.length);
            self.span(spans.bitmap);
        }
        self.len(columns.dictionary_spans.len());
        for (column, spans) in &columns.dictionary_spans {
            self.u32(*column);
            self.span(spans.count);
            self.len(spans.entries.len());
            for entry in &spans.entries {
                self.string_spans(*entry);
            }
            self.span(spans.width);
            self.span(spans.reserved);
        }
        self.len(columns.cells.len());
        for ((row, column), cell) in &columns.cells {
            self.u32(*row);
            self.u32(*column);
            self.boolean(cell.present);
            self.scalar(&cell.logical);
            self.physical(&cell.physical);
            self.span(cell.span);
            match cell.length_span {
                None => self.tag(0),
                Some(span) => {
                    self.tag(1);
                    self.span(span);
                }
            }
            self.span(cell.payload_span);
        }
    }

    fn source_range(&mut self, range: SourceRangeDto) {
        match range {
            SourceRangeDto::Unstamped => self.tag(0),
            SourceRangeDto::Empty => self.tag(1),
            SourceRangeDto::Bounded { min, max } => {
                self.tag(2);
                self.i64(min);
                self.i64(max);
            }
        }
    }

    fn exact_hit(&mut self, hit: &ExactHitDto) {
        self.string(&hit.source);
        self.u32(hit.row_id);
        self.u128(hit.document_id);
        self.u32(hit.distance_bits);
    }

    fn exact_hits(&mut self, hits: &[ExactHitDto]) {
        self.len(hits.len());
        for hit in hits {
            self.exact_hit(hit);
        }
    }

    fn reports(&mut self, reports: &[BranchReportDto]) {
        self.len(reports.len());
        for report in reports {
            self.report(report);
        }
    }

    fn receipts(&mut self, receipts: &[ExecutionReceiptDto]) {
        self.len(receipts.len());
        for receipt in receipts {
            self.receipt(receipt);
        }
    }

    fn i39_case(&mut self, case: &I39ExpectedCase) {
        self.key(&case.key);
        match case.mode {
            I39ExecutionModeDto::ExactScan { source_may_match } => {
                self.tag(0);
                self.boolean(source_may_match);
            }
            I39ExecutionModeDto::FilteredGraph { required_fallback } => {
                self.tag(1);
                self.fallback(required_fallback);
            }
        }
        self.u64(case.row_count);
        self.u64(case.filter_cardinality);
        self.u64(case.allow_list_threshold);
        self.u64(case.rows_examined);
        self.u64(case.allowed_rows_examined);
        self.u64(case.vectors_scored);
        self.u64(case.graph_nodes_visited);
        self.u64(case.exact_fallback_rows_examined);
        self.u64(case.returned_candidates);
        self.optional_u64(case.ef_effective);
        self.optional_u64(case.visited_budget);
        self.boolean(case.sealed);
    }
}

/// Result of replaying one retained canonical metadata comparison without any
/// production dependency or seed-derived fixture regeneration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetadataCanonicalReplay {
    pub checker_id: &'static str,
    pub input_digest: u64,
    pub observed_digest: u64,
    pub first_difference: Option<FirstDifference>,
}

/// Decode retained family canonical bytes, reject noncanonical encodings, and
/// rerun the exact independent checker selected by `checker_id`.
pub fn replay_canonical_comparison(
    checker_id: &str,
    input_bytes: &[u8],
    observed_bytes: &[u8],
) -> Result<MetadataCanonicalReplay, String> {
    let first_difference = match checker_id {
        I36_CHECKER_ID => {
            let input = decode_i36_input(input_bytes)?;
            let observed = decode_i36_observed(observed_bytes)?;
            if canonical_i36_input_bytes(&input) != input_bytes
                || canonical_i36_observed_bytes(&observed) != observed_bytes
            {
                return Err("metadata I36 retained bytes are not canonical".to_owned());
            }
            first_i36_difference(&input, &observed)
        }
        I37_CHECKER_ID => {
            let input = decode_i37_input(input_bytes)?;
            let observed = decode_i37_observed(observed_bytes)?;
            if canonical_i37_input_bytes(&input) != input_bytes
                || canonical_i37_observed_bytes(&observed) != observed_bytes
            {
                return Err("metadata I37 retained bytes are not canonical".to_owned());
            }
            first_i37_difference(&input, &observed)
        }
        I38_CHECKER_ID => {
            let input = decode_i38_input(input_bytes)?;
            let observed = decode_i38_observed(observed_bytes)?;
            if canonical_i38_input_bytes(&input) != input_bytes
                || canonical_i38_observed_bytes(&observed) != observed_bytes
            {
                return Err("metadata I38 retained bytes are not canonical".to_owned());
            }
            first_i38_difference(&input, &observed)
        }
        I39_CHECKER_ID => {
            let input = decode_i39_input(input_bytes)?;
            let observed = decode_i39_observed(observed_bytes)?;
            if canonical_i39_input_bytes(&input) != input_bytes
                || canonical_i39_observed_bytes(&observed) != observed_bytes
            {
                return Err("metadata I39 retained bytes are not canonical".to_owned());
            }
            first_i39_difference(&input, &observed)
        }
        other => return Err(format!("unknown metadata canonical checker {other}")),
    };
    Ok(MetadataCanonicalReplay {
        checker_id: match checker_id {
            I36_CHECKER_ID => I36_CHECKER_ID,
            I37_CHECKER_ID => I37_CHECKER_ID,
            I38_CHECKER_ID => I38_CHECKER_ID,
            I39_CHECKER_ID => I39_CHECKER_ID,
            _ => unreachable!("checker was matched above"),
        },
        input_digest: canonical_digest(input_bytes),
        observed_digest: canonical_digest(observed_bytes),
        first_difference,
    })
}

struct CanonicalReader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> CanonicalReader<'a> {
    fn new(bytes: &'a [u8], domain: &[u8]) -> Result<Self, String> {
        let mut reader = Self { bytes, position: 0 };
        if reader.blob()? != domain {
            return Err("metadata canonical domain mismatch".to_owned());
        }
        Ok(reader)
    }

    fn finish(self) -> Result<(), String> {
        if self.position == self.bytes.len() {
            Ok(())
        } else {
            Err(format!(
                "metadata canonical bytes have {} trailing bytes",
                self.bytes.len().saturating_sub(self.position)
            ))
        }
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], String> {
        let end = self
            .position
            .checked_add(length)
            .ok_or_else(|| "metadata canonical offset overflowed".to_owned())?;
        let bytes = self
            .bytes
            .get(self.position..end)
            .ok_or_else(|| "metadata canonical bytes are truncated".to_owned())?;
        self.position = end;
        Ok(bytes)
    }

    fn tag(&mut self) -> Result<u8, String> {
        Ok(*self.take(1)?.first().expect("one-byte canonical tag"))
    }

    fn boolean(&mut self) -> Result<bool, String> {
        match self.tag()? {
            0 => Ok(false),
            1 => Ok(true),
            tag => Err(format!("metadata canonical Boolean tag {tag} is invalid")),
        }
    }

    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(
            self.take(4)?
                .try_into()
                .expect("canonical u32 has four bytes"),
        ))
    }

    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(
            self.take(8)?
                .try_into()
                .expect("canonical u64 has eight bytes"),
        ))
    }

    fn i64(&mut self) -> Result<i64, String> {
        Ok(i64::from_le_bytes(
            self.take(8)?
                .try_into()
                .expect("canonical i64 has eight bytes"),
        ))
    }

    fn u128(&mut self) -> Result<u128, String> {
        Ok(u128::from_le_bytes(
            self.take(16)?
                .try_into()
                .expect("canonical u128 has sixteen bytes"),
        ))
    }

    fn len(&mut self) -> Result<usize, String> {
        usize::try_from(self.u64()?)
            .map_err(|_| "metadata canonical length exceeds usize".to_owned())
    }

    fn blob(&mut self) -> Result<&'a [u8], String> {
        let length = self.len()?;
        self.take(length)
    }

    fn string(&mut self) -> Result<String, String> {
        String::from_utf8(self.blob()?.to_vec())
            .map_err(|error| format!("metadata canonical string is not UTF-8: {error}"))
    }

    fn scalar(&mut self) -> Result<ScalarCell, String> {
        match self.tag()? {
            0 => Ok(ScalarCell::Null),
            1 => Ok(ScalarCell::U64(self.u64()?)),
            2 => Ok(ScalarCell::I64(self.i64()?)),
            3 => Ok(ScalarCell::F64Bits(self.u64()?)),
            4 => Ok(ScalarCell::Bool(self.boolean()?)),
            5 => Ok(ScalarCell::Utf8(self.blob()?.to_vec())),
            tag => Err(format!("metadata canonical scalar tag {tag} is invalid")),
        }
    }

    fn column_kind(&mut self) -> Result<ColumnKind, String> {
        match self.tag()? {
            0 => Ok(ColumnKind::U64),
            1 => Ok(ColumnKind::I64),
            2 => Ok(ColumnKind::F64),
            3 => Ok(ColumnKind::Bool),
            4 => Ok(ColumnKind::DictionaryString),
            5 => Ok(ColumnKind::RawString),
            tag => Err(format!(
                "metadata canonical column-kind tag {tag} is invalid"
            )),
        }
    }

    fn definitions(&mut self) -> Result<Vec<ColumnDefinitionDto>, String> {
        (0..self.len()?)
            .map(|_| {
                Ok(ColumnDefinitionDto {
                    id: self.u32()?,
                    name: self.blob()?.to_vec(),
                    kind: self.column_kind()?,
                    nullable: self.boolean()?,
                })
            })
            .collect()
    }

    fn cells(&mut self) -> Result<BTreeMap<u32, ScalarCell>, String> {
        let mut cells = BTreeMap::new();
        for _ in 0..self.len()? {
            let column = self.u32()?;
            let value = self.scalar()?;
            if cells.insert(column, value).is_some() {
                return Err(format!("metadata canonical duplicate column {column}"));
            }
        }
        Ok(cells)
    }

    fn logical_rows(&mut self) -> Result<Vec<BTreeMap<u32, ScalarCell>>, String> {
        (0..self.len()?).map(|_| self.cells()).collect()
    }

    fn row(&mut self) -> Result<MetadataRowDto, String> {
        Ok(MetadataRowDto {
            row_id: self.u32()?,
            cells: self.cells()?,
        })
    }

    fn rows(&mut self) -> Result<Vec<MetadataRowDto>, String> {
        (0..self.len()?).map(|_| self.row()).collect()
    }

    fn set(&mut self) -> Result<BTreeSet<u32>, String> {
        let mut rows = BTreeSet::new();
        for _ in 0..self.len()? {
            let row = self.u32()?;
            if !rows.insert(row) {
                return Err(format!("metadata canonical duplicate row {row}"));
            }
        }
        Ok(rows)
    }

    fn u32_slice(&mut self) -> Result<Vec<u32>, String> {
        (0..self.len()?).map(|_| self.u32()).collect()
    }

    fn bound(&mut self) -> Result<Option<RangeBoundDto>, String> {
        match self.tag()? {
            0 => Ok(None),
            1 => Ok(Some(RangeBoundDto {
                value: self.scalar()?,
                inclusive: self.boolean()?,
            })),
            tag => Err(format!(
                "metadata canonical range-bound tag {tag} is invalid"
            )),
        }
    }

    fn predicate(&mut self) -> Result<PredicateDto, String> {
        match self.tag()? {
            0 => Ok(PredicateDto::Eq {
                column: self.u32()?,
                value: self.scalar()?,
            }),
            1 => {
                let column = self.u32()?;
                let values = (0..self.len()?)
                    .map(|_| self.scalar())
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(PredicateDto::In { column, values })
            }
            2 => Ok(PredicateDto::Range {
                column: self.u32()?,
                lower: self.bound()?,
                upper: self.bound()?,
            }),
            3 => Ok(PredicateDto::Exists(self.u32()?)),
            4 => Ok(PredicateDto::IsNull(self.u32()?)),
            5 | 6 => {
                let tag = self.bytes[self.position - 1];
                let children = (0..self.len()?)
                    .map(|_| self.predicate())
                    .collect::<Result<Vec<_>, _>>()?;
                if tag == 5 {
                    Ok(PredicateDto::And(children))
                } else {
                    Ok(PredicateDto::Or(children))
                }
            }
            7 => Ok(PredicateDto::Not(Box::new(self.predicate()?))),
            tag => Err(format!("metadata canonical predicate tag {tag} is invalid")),
        }
    }

    fn span(&mut self) -> Result<ByteSpan, String> {
        Ok(ByteSpan {
            start: self.len()?,
            end: self.len()?,
        })
    }

    fn string_spans(&mut self) -> Result<StringByteSpans, String> {
        Ok(StringByteSpans {
            full: self.span()?,
            length: self.span()?,
            payload: self.span()?,
        })
    }

    fn physical(&mut self) -> Result<PhysicalCell, String> {
        match self.tag()? {
            0 => Ok(PhysicalCell::U64(self.u64()?)),
            1 => Ok(PhysicalCell::I64(self.i64()?)),
            2 => Ok(PhysicalCell::F64Bits(self.u64()?)),
            3 => Ok(PhysicalCell::BoolByte(self.tag()?)),
            4 => {
                let code = self.u32()?;
                let decoded = match self.tag()? {
                    0 => None,
                    1 => Some(self.blob()?.to_vec()),
                    tag => {
                        return Err(format!(
                            "metadata canonical dictionary decoded tag {tag} is invalid"
                        ));
                    }
                };
                Ok(PhysicalCell::DictionaryCode { code, decoded })
            }
            5 => Ok(PhysicalCell::RawUtf8(self.blob()?.to_vec())),
            tag => Err(format!(
                "metadata canonical physical-cell tag {tag} is invalid"
            )),
        }
    }

    fn parsed_columns(&mut self) -> Result<ParsedColumns, String> {
        let row_count = self.u32()?;
        let definitions = self.definitions()?;
        let row_count_span = self.span()?;
        let column_count_span = self.span()?;
        let mut definition_spans = BTreeMap::new();
        for _ in 0..self.len()? {
            let column = self.u32()?;
            let spans = DefinitionByteSpans {
                full: self.span()?,
                id: self.span()?,
                kind: self.span()?,
                nullable: self.span()?,
                name: self.string_spans()?,
            };
            if definition_spans.insert(column, spans).is_some() {
                return Err(format!(
                    "metadata canonical duplicate definition span {column}"
                ));
            }
        }
        let mut presence_spans = BTreeMap::new();
        for _ in 0..self.len()? {
            let column = self.u32()?;
            let spans = PresenceByteSpans {
                length: self.span()?,
                bitmap: self.span()?,
            };
            if presence_spans.insert(column, spans).is_some() {
                return Err(format!(
                    "metadata canonical duplicate presence span {column}"
                ));
            }
        }
        let mut dictionary_spans = BTreeMap::new();
        for _ in 0..self.len()? {
            let column = self.u32()?;
            let count = self.span()?;
            let entries = (0..self.len()?)
                .map(|_| self.string_spans())
                .collect::<Result<Vec<_>, _>>()?;
            let spans = DictionaryByteSpans {
                count,
                entries,
                width: self.span()?,
                reserved: self.span()?,
            };
            if dictionary_spans.insert(column, spans).is_some() {
                return Err(format!(
                    "metadata canonical duplicate dictionary span {column}"
                ));
            }
        }
        let mut cells = BTreeMap::new();
        for _ in 0..self.len()? {
            let row = self.u32()?;
            let column = self.u32()?;
            let present = self.boolean()?;
            let logical = self.scalar()?;
            let physical = self.physical()?;
            let span = self.span()?;
            let length_span = match self.tag()? {
                0 => None,
                1 => Some(self.span()?),
                tag => {
                    return Err(format!(
                        "metadata canonical optional span tag {tag} is invalid"
                    ));
                }
            };
            let payload_span = self.span()?;
            let cell = ParsedCell {
                present,
                logical,
                physical,
                span,
                length_span,
                payload_span,
            };
            if cells.insert((row, column), cell).is_some() {
                return Err(format!(
                    "metadata canonical duplicate parsed cell row={row} column={column}"
                ));
            }
        }
        Ok(ParsedColumns {
            row_count,
            definitions,
            row_count_span,
            column_count_span,
            definition_spans,
            presence_spans,
            dictionary_spans,
            cells,
        })
    }

    fn branch(&mut self) -> Result<ExecutionBranchDto, String> {
        match self.tag()? {
            0 => Ok(ExecutionBranchDto::Pruned),
            1 => Ok(ExecutionBranchDto::ExactAllowList),
            2 => Ok(ExecutionBranchDto::MaskedScan),
            3 => Ok(ExecutionBranchDto::FilteredGraph),
            4 => Ok(ExecutionBranchDto::GraphExactFallback),
            tag => Err(format!("metadata canonical branch tag {tag} is invalid")),
        }
    }

    fn fallback(&mut self) -> Result<FallbackReasonDto, String> {
        match self.tag()? {
            0 => Ok(FallbackReasonDto::None),
            1 => Ok(FallbackReasonDto::VisitedBudget),
            2 => Ok(FallbackReasonDto::CandidateShortfall),
            3 => Ok(FallbackReasonDto::EfWidened),
            tag => Err(format!("metadata canonical fallback tag {tag} is invalid")),
        }
    }

    fn key(&mut self) -> Result<QuerySourceKey, String> {
        Ok(QuerySourceKey {
            query_id: self.u64()?,
            source: self.string()?,
        })
    }

    fn report(&mut self) -> Result<BranchReportDto, String> {
        Ok(BranchReportDto {
            key: self.key()?,
            branch: self.branch()?,
            fallback: self.fallback()?,
            filter_cardinality: self.u64()?,
        })
    }

    fn optional_u64(&mut self) -> Result<Option<u64>, String> {
        match self.tag()? {
            0 => Ok(None),
            1 => Ok(Some(self.u64()?)),
            tag => Err(format!(
                "metadata canonical optional-u64 tag {tag} is invalid"
            )),
        }
    }

    fn receipt(&mut self) -> Result<ExecutionReceiptDto, String> {
        Ok(ExecutionReceiptDto {
            key: self.key()?,
            branch: self.branch()?,
            fallback: self.fallback()?,
            row_count: self.u64()?,
            filter_cardinality: self.u64()?,
            rows_examined: self.u64()?,
            allowed_rows_examined: self.u64()?,
            vectors_scored: self.u64()?,
            graph_nodes_visited: self.u64()?,
            exact_fallback_rows_examined: self.u64()?,
            returned_candidates: self.u64()?,
            ef_effective: self.optional_u64()?,
            visited_budget: self.optional_u64()?,
            sealed: self.boolean()?,
        })
    }

    fn reports(&mut self) -> Result<Vec<BranchReportDto>, String> {
        (0..self.len()?).map(|_| self.report()).collect()
    }

    fn receipts(&mut self) -> Result<Vec<ExecutionReceiptDto>, String> {
        (0..self.len()?).map(|_| self.receipt()).collect()
    }

    fn source_range(&mut self) -> Result<SourceRangeDto, String> {
        match self.tag()? {
            0 => Ok(SourceRangeDto::Unstamped),
            1 => Ok(SourceRangeDto::Empty),
            2 => Ok(SourceRangeDto::Bounded {
                min: self.i64()?,
                max: self.i64()?,
            }),
            tag => Err(format!(
                "metadata canonical source-range tag {tag} is invalid"
            )),
        }
    }

    fn exact_hit(&mut self) -> Result<ExactHitDto, String> {
        Ok(ExactHitDto {
            source: self.string()?,
            row_id: self.u32()?,
            document_id: self.u128()?,
            distance_bits: self.u32()?,
        })
    }

    fn exact_hits(&mut self) -> Result<Vec<ExactHitDto>, String> {
        (0..self.len()?).map(|_| self.exact_hit()).collect()
    }

    fn i39_case(&mut self) -> Result<I39ExpectedCase, String> {
        let key = self.key()?;
        let mode = match self.tag()? {
            0 => I39ExecutionModeDto::ExactScan {
                source_may_match: self.boolean()?,
            },
            1 => I39ExecutionModeDto::FilteredGraph {
                required_fallback: self.fallback()?,
            },
            tag => return Err(format!("metadata canonical I39 mode tag {tag} is invalid")),
        };
        Ok(I39ExpectedCase {
            key,
            mode,
            row_count: self.u64()?,
            filter_cardinality: self.u64()?,
            allow_list_threshold: self.u64()?,
            rows_examined: self.u64()?,
            allowed_rows_examined: self.u64()?,
            vectors_scored: self.u64()?,
            graph_nodes_visited: self.u64()?,
            exact_fallback_rows_examined: self.u64()?,
            returned_candidates: self.u64()?,
            ef_effective: self.optional_u64()?,
            visited_budget: self.optional_u64()?,
            sealed: self.boolean()?,
        })
    }
}

fn decode_i36_input(bytes: &[u8]) -> Result<I36Input, String> {
    let mut reader = CanonicalReader::new(bytes, b"metadata/I36/input/v1")?;
    let input = I36Input {
        source: reader.string()?,
        definitions: reader.definitions()?,
        rows: reader.logical_rows()?,
    };
    reader.finish()?;
    Ok(input)
}

fn decode_i36_observed(bytes: &[u8]) -> Result<I36Observed, String> {
    let mut reader = CanonicalReader::new(bytes, b"metadata/I36/observed/v1")?;
    let observed = I36Observed {
        active_rows: reader.logical_rows()?,
        raw: reader.parsed_columns()?,
        reader_rows: reader.logical_rows()?,
        public_rows: reader.logical_rows()?,
    };
    reader.finish()?;
    Ok(observed)
}

fn decode_i37_input(bytes: &[u8]) -> Result<I37Input, String> {
    let mut reader = CanonicalReader::new(bytes, b"metadata/I37/input/v1")?;
    let rows = reader.rows()?;
    let live = reader.set()?;
    let predicate = reader.predicate()?;
    let sources = (0..reader.len()?)
        .map(|_| {
            Ok(I37SourceInputDto {
                source: reader.string()?,
                sealed: reader.boolean()?,
                rows: reader.rows()?,
                live: reader.set()?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let input = I37Input {
        rows,
        live,
        predicate,
        sources,
    };
    reader.finish()?;
    Ok(input)
}

/// Decodes and canonical-round-trips one retained I37 input for product replay.
pub fn decode_canonical_i37_input(bytes: &[u8]) -> Result<I37Input, String> {
    let input = decode_i37_input(bytes)?;
    if canonical_i37_input_bytes(&input) != bytes {
        return Err("metadata I37 retained input bytes are not canonical".to_owned());
    }
    Ok(input)
}

fn decode_i37_observed(bytes: &[u8]) -> Result<I37Observed, String> {
    let mut reader = CanonicalReader::new(bytes, b"metadata/I37/observed/v1")?;
    let evaluator = reader.set()?;
    let public_results = reader.u32_slice()?;
    let sources = (0..reader.len()?)
        .map(|_| {
            Ok(I37SourceObservedDto {
                source: reader.string()?,
                sealed: reader.boolean()?,
                row_count: reader.u32()?,
                live: reader.set()?,
                evaluator: reader.set()?,
                public_results: reader.u32_slice()?,
                report: reader.report()?,
                receipt: reader.receipt()?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let observed = I37Observed {
        evaluator,
        public_results,
        sources,
        allow_list_threshold: reader.u64()?,
    };
    reader.finish()?;
    Ok(observed)
}

fn decode_i38_input(bytes: &[u8]) -> Result<I38Input, String> {
    let mut reader = CanonicalReader::new(bytes, b"metadata/I38/input/v1")?;
    let sources = (0..reader.len()?)
        .map(|_| {
            Ok(I38SourceDto {
                source: reader.string()?,
                sealed: reader.boolean()?,
                range: reader.source_range()?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let rows = (0..reader.len()?)
        .map(|_| {
            Ok(SourceMetadataRowDto {
                source: reader.string()?,
                row_id: reader.u32()?,
                document_id: reader.u128()?,
                cells: reader.cells()?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let mut live = BTreeSet::new();
    for _ in 0..reader.len()? {
        let key = (reader.string()?, reader.u32()?);
        if !live.insert(key.clone()) {
            return Err(format!(
                "metadata canonical duplicate live source/row {}:{}",
                key.0, key.1
            ));
        }
    }
    let input = I38Input {
        sources,
        rows,
        live,
        predicate: reader.predicate()?,
        unfiltered_exact: reader.exact_hits()?,
        expected_delete_records: reader.u64()?,
    };
    reader.finish()?;
    Ok(input)
}

/// Decodes and canonical-round-trips one retained I38 input for product replay.
pub fn decode_canonical_i38_input(bytes: &[u8]) -> Result<I38Input, String> {
    let input = decode_i38_input(bytes)?;
    if canonical_i38_input_bytes(&input) != bytes {
        return Err("metadata I38 retained input bytes are not canonical".to_owned());
    }
    Ok(input)
}

fn decode_i38_observed(bytes: &[u8]) -> Result<I38Observed, String> {
    let mut reader = CanonicalReader::new(bytes, b"metadata/I38/observed/v1")?;
    let filtered_exact = reader.exact_hits()?;
    let mut pruned_sources = BTreeSet::new();
    for _ in 0..reader.len()? {
        let source = reader.string()?;
        if !pruned_sources.insert(source.clone()) {
            return Err(format!(
                "metadata canonical duplicate pruned source {source}"
            ));
        }
    }
    let observed = I38Observed {
        filtered_exact,
        pruned_sources,
        reports: reader.reports()?,
        execution_receipts: reader.receipts()?,
        allow_list_threshold: reader.u64()?,
        wal_delete_records: reader.u64()?,
    };
    reader.finish()?;
    Ok(observed)
}

fn decode_i39_input(bytes: &[u8]) -> Result<Vec<I39ExpectedCase>, String> {
    let mut reader = CanonicalReader::new(bytes, b"metadata/I39/input/v1")?;
    let cases = (0..reader.len()?)
        .map(|_| reader.i39_case())
        .collect::<Result<Vec<_>, _>>()?;
    reader.finish()?;
    Ok(cases)
}

fn decode_i39_observed(bytes: &[u8]) -> Result<I39Observed, String> {
    let mut reader = CanonicalReader::new(bytes, b"metadata/I39/observed/v1")?;
    let observed = I39Observed {
        reports: reader.reports()?,
        diagnostics_reports: reader.reports()?,
        receipts: reader.receipts()?,
        allow_list_threshold: reader.u64()?,
    };
    reader.finish()?;
    Ok(observed)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I37SourceInputDto {
    pub source: String,
    pub sealed: bool,
    pub rows: Vec<MetadataRowDto>,
    pub live: BTreeSet<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I37SourceObservedDto {
    pub source: String,
    pub sealed: bool,
    pub row_count: u32,
    pub live: BTreeSet<u32>,
    pub evaluator: BTreeSet<u32>,
    pub public_results: Vec<u32>,
    pub report: BranchReportDto,
    pub receipt: ExecutionReceiptDto,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AlgebraResult {
    pub result: BTreeSet<u32>,
    pub ledger: BTreeMap<String, BTreeSet<u32>>,
}

pub fn evaluate_i37(input: &I37Input) -> Result<AlgebraResult, String> {
    let rows = input
        .rows
        .iter()
        .map(|row| (row.row_id, row))
        .collect::<BTreeMap<_, _>>();
    if rows.len() != input.rows.len() {
        return Err(format!("{I37_CHECKER_ID} duplicate row id in input"));
    }
    if let Some(missing) = input.live.iter().find(|row| !rows.contains_key(row)) {
        return Err(format!(
            "{I37_CHECKER_ID} live row {missing} is absent from input rows"
        ));
    }
    let mut ledger = BTreeMap::new();
    let result = eval_predicate_dto(&input.predicate, &rows, &input.live, "root", &mut ledger);
    Ok(AlgebraResult { result, ledger })
}

pub fn compare_i37(input: &I37Input, observed: &I37Observed) -> Result<(), String> {
    let expected = evaluate_i37(input)?;
    let path = predicate_path("root", &input.predicate);
    compare_set(&path, &expected.result, &observed.evaluator)?;

    let public = observed
        .public_results
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    if public.len() != observed.public_results.len() {
        let mut seen = BTreeSet::new();
        let duplicate = observed
            .public_results
            .iter()
            .copied()
            .find(|row| !seen.insert(*row))
            .unwrap_or_default();
        return Err(format!(
            "{I37_CHECKER_ID} path={path} duplicate_row={duplicate}"
        ));
    }
    if let Some(dead) = public.iter().find(|row| !input.live.contains(row)) {
        return Err(format!("{I37_CHECKER_ID} path={path} dead_row={dead}"));
    }
    compare_set(&path, &expected.result, &public)?;

    let expected_sources = input
        .sources
        .iter()
        .map(|source| (source.source.clone(), source))
        .collect::<BTreeMap<_, _>>();
    if expected_sources.len() != input.sources.len() {
        return Err(format!("{I37_CHECKER_ID} duplicate source input"));
    }
    let observed_sources = observed
        .sources
        .iter()
        .map(|source| (source.source.clone(), source))
        .collect::<BTreeMap<_, _>>();
    if observed_sources.len() != observed.sources.len() {
        return Err(format!("{I37_CHECKER_ID} duplicate source observation"));
    }
    if expected_sources.len() != observed_sources.len() {
        return Err(format!(
            "{I37_CHECKER_ID} source count expected={} observed={}",
            expected_sources.len(),
            observed_sources.len()
        ));
    }
    for (source_name, source_input) in expected_sources {
        let source_observed = observed_sources.get(&source_name).ok_or_else(|| {
            format!("{I37_CHECKER_ID} source={source_name} missing source observation")
        })?;
        if source_input.sealed != source_observed.sealed {
            return Err(format!(
                "{I37_CHECKER_ID} source={source_name} expected sealed={} observed sealed={}",
                source_input.sealed, source_observed.sealed
            ));
        }
        let expected_row_count = u32::try_from(source_input.rows.len())
            .map_err(|_| format!("{I37_CHECKER_ID} source row count exceeds u32"))?;
        if source_observed.row_count != expected_row_count
            || source_observed.live != source_input.live
        {
            return Err(format!(
                "{I37_CHECKER_ID} source={source_name} expected row_count/live={expected_row_count}/{:?} observed={}/{:?}",
                source_input.live, source_observed.row_count, source_observed.live
            ));
        }
        let source_expected =
            evaluate_i37_rows(&input.predicate, &source_input.rows, &source_input.live)?;
        let source_path = format!("source={source_name}/{path}");
        compare_set(
            &source_path,
            &source_expected.result,
            &source_observed.evaluator,
        )?;
        let source_public = source_observed
            .public_results
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        if source_public.len() != source_observed.public_results.len() {
            return Err(format!(
                "{I37_CHECKER_ID} {source_path} duplicate public row"
            ));
        }
        compare_set(&source_path, &source_expected.result, &source_public)?;
        if source_observed.report.key.source != source_name
            || source_observed.receipt.key.source != source_name
            || source_observed.report.key != source_observed.receipt.key
        {
            return Err(format!(
                "{I37_CHECKER_ID} source={source_name} report/receipt correlation mismatch"
            ));
        }
        if source_observed.report.branch != source_observed.receipt.branch
            || source_observed.report.fallback != source_observed.receipt.fallback
            || source_observed.report.filter_cardinality
                != source_observed.receipt.filter_cardinality
        {
            return Err(format!(
                "{I37_CHECKER_ID} source={source_name} report/receipt execution mismatch"
            ));
        }
        if source_observed.receipt.sealed != source_input.sealed {
            return Err(format!(
                "{I37_CHECKER_ID} source={source_name} receipt sealed={} disagrees with lifecycle",
                source_observed.receipt.sealed
            ));
        }
        let expected_cardinality = u64::try_from(source_expected.result.len())
            .map_err(|_| format!("{I37_CHECKER_ID} source result count exceeds u64"))?;
        if source_observed.receipt.filter_cardinality != expected_cardinality {
            return Err(format!(
                "{I37_CHECKER_ID} source={source_name} filter cardinality expected={expected_cardinality} observed={}",
                source_observed.receipt.filter_cardinality
            ));
        }
        validate_receipt_semantics(&source_observed.receipt, observed.allow_list_threshold)?;
    }
    Ok(())
}

fn evaluate_i37_rows(
    predicate: &PredicateDto,
    rows: &[MetadataRowDto],
    live: &BTreeSet<u32>,
) -> Result<AlgebraResult, String> {
    evaluate_i37(&I37Input {
        rows: rows.to_vec(),
        live: live.clone(),
        predicate: predicate.clone(),
        sources: Vec::new(),
    })
}

fn compare_set(
    path: &str,
    expected: &BTreeSet<u32>,
    observed: &BTreeSet<u32>,
) -> Result<(), String> {
    if let Some(row) = observed.difference(expected).next() {
        return Err(format!("{I37_CHECKER_ID} path={path} extra_row={row}"));
    }
    if let Some(row) = expected.difference(observed).next() {
        return Err(format!("{I37_CHECKER_ID} path={path} missing_row={row}"));
    }
    Ok(())
}

fn eval_predicate_dto(
    predicate: &PredicateDto,
    rows: &BTreeMap<u32, &MetadataRowDto>,
    live: &BTreeSet<u32>,
    parent_path: &str,
    ledger: &mut BTreeMap<String, BTreeSet<u32>>,
) -> BTreeSet<u32> {
    let path = predicate_path(parent_path, predicate);
    let result = match predicate {
        PredicateDto::Eq { column, value } => live
            .iter()
            .copied()
            .filter(|row| {
                rows.get(row)
                    .and_then(|row| row.cells.get(column))
                    .is_some_and(|stored| scalar_equal(stored, value))
            })
            .collect(),
        PredicateDto::In { column, values } => live
            .iter()
            .copied()
            .filter(|row| {
                rows.get(row)
                    .and_then(|row| row.cells.get(column))
                    .is_some_and(|stored| values.iter().any(|value| scalar_equal(stored, value)))
            })
            .collect(),
        PredicateDto::Range {
            column,
            lower,
            upper,
        } => live
            .iter()
            .copied()
            .filter(|row| {
                rows.get(row)
                    .and_then(|row| row.cells.get(column))
                    .is_some_and(|stored| scalar_in_range(stored, lower.as_ref(), upper.as_ref()))
            })
            .collect(),
        PredicateDto::Exists(column) => live
            .iter()
            .copied()
            .filter(|row| {
                rows.get(row)
                    .and_then(|row| row.cells.get(column))
                    .is_some_and(|cell| *cell != ScalarCell::Null)
            })
            .collect(),
        PredicateDto::IsNull(column) => live
            .iter()
            .copied()
            .filter(|row| {
                rows.get(row)
                    .and_then(|row| row.cells.get(column))
                    .is_none_or(|cell| *cell == ScalarCell::Null)
            })
            .collect(),
        PredicateDto::And(children) => {
            let mut result = live.clone();
            for (index, child) in children.iter().enumerate() {
                let child_parent = format!("{path}[{index}]");
                let child_result = eval_predicate_dto(child, rows, live, &child_parent, ledger);
                result = result.intersection(&child_result).copied().collect();
            }
            result
        }
        PredicateDto::Or(children) => {
            let mut result = BTreeSet::new();
            for (index, child) in children.iter().enumerate() {
                let child_parent = format!("{path}[{index}]");
                let child_result = eval_predicate_dto(child, rows, live, &child_parent, ledger);
                result = result.union(&child_result).copied().collect();
            }
            result
        }
        PredicateDto::Not(child) => {
            let child_result = eval_predicate_dto(child, rows, live, &path, ledger);
            live.difference(&child_result).copied().collect()
        }
    };
    ledger.insert(path, result.clone());
    result
}

fn predicate_path(parent: &str, predicate: &PredicateDto) -> String {
    let node = match predicate {
        PredicateDto::Eq { column, .. } => format!("Eq(column={column})"),
        PredicateDto::In { column, .. } => format!("In(column={column})"),
        PredicateDto::Range { column, .. } => format!("Range(column={column})"),
        PredicateDto::Exists(column) => format!("Exists(column={column})"),
        PredicateDto::IsNull(column) => format!("IsNull(column={column})"),
        PredicateDto::And(_) => "And".to_owned(),
        PredicateDto::Or(_) => "Or".to_owned(),
        PredicateDto::Not(_) => "Not".to_owned(),
    };
    format!("{parent}/{node}")
}

fn scalar_equal(stored: &ScalarCell, query: &ScalarCell) -> bool {
    match (stored, query) {
        (ScalarCell::U64(left), ScalarCell::U64(right)) => left == right,
        (ScalarCell::I64(left), ScalarCell::I64(right)) => left == right,
        (ScalarCell::F64Bits(left), ScalarCell::F64Bits(right)) => {
            f64::from_bits(*left) == f64::from_bits(*right)
        }
        (ScalarCell::Bool(left), ScalarCell::Bool(right)) => left == right,
        (ScalarCell::Utf8(left), ScalarCell::Utf8(right)) => left == right,
        (ScalarCell::Null, _)
        | (_, ScalarCell::Null)
        | (ScalarCell::U64(_), _)
        | (ScalarCell::I64(_), _)
        | (ScalarCell::F64Bits(_), _)
        | (ScalarCell::Bool(_), _)
        | (ScalarCell::Utf8(_), _) => false,
    }
}

fn scalar_in_range(
    stored: &ScalarCell,
    lower: Option<&RangeBoundDto>,
    upper: Option<&RangeBoundDto>,
) -> bool {
    scalar_bound(stored, lower, true) && scalar_bound(stored, upper, false)
}

fn scalar_bound(stored: &ScalarCell, bound: Option<&RangeBoundDto>, lower: bool) -> bool {
    let Some(bound) = bound else {
        return !matches!(stored, ScalarCell::Null)
            && !matches!(stored, ScalarCell::F64Bits(bits) if f64::from_bits(*bits).is_nan());
    };
    match (stored, &bound.value) {
        (ScalarCell::U64(value), ScalarCell::U64(endpoint)) => {
            compare_ordered(*value, *endpoint, bound.inclusive, lower)
        }
        (ScalarCell::I64(value), ScalarCell::I64(endpoint)) => {
            compare_ordered(*value, *endpoint, bound.inclusive, lower)
        }
        (ScalarCell::F64Bits(value), ScalarCell::F64Bits(endpoint)) => {
            let value = f64::from_bits(*value);
            let endpoint = f64::from_bits(*endpoint);
            !value.is_nan()
                && !endpoint.is_nan()
                && compare_ordered(value, endpoint, bound.inclusive, lower)
        }
        _ => false,
    }
}

fn compare_ordered<T: PartialOrd>(value: T, endpoint: T, inclusive: bool, lower: bool) -> bool {
    match (lower, inclusive) {
        (true, true) => value >= endpoint,
        (true, false) => value > endpoint,
        (false, true) => value <= endpoint,
        (false, false) => value < endpoint,
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ExecutionBranchDto {
    Pruned,
    ExactAllowList,
    MaskedScan,
    FilteredGraph,
    GraphExactFallback,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FallbackReasonDto {
    None,
    VisitedBudget,
    CandidateShortfall,
    EfWidened,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct QuerySourceKey {
    pub query_id: u64,
    pub source: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BranchReportDto {
    pub key: QuerySourceKey,
    pub branch: ExecutionBranchDto,
    pub fallback: FallbackReasonDto,
    pub filter_cardinality: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionReceiptDto {
    pub key: QuerySourceKey,
    pub branch: ExecutionBranchDto,
    pub fallback: FallbackReasonDto,
    pub row_count: u64,
    pub filter_cardinality: u64,
    pub rows_examined: u64,
    pub allowed_rows_examined: u64,
    pub vectors_scored: u64,
    pub graph_nodes_visited: u64,
    pub exact_fallback_rows_examined: u64,
    pub returned_candidates: u64,
    pub ef_effective: Option<u64>,
    pub visited_budget: Option<u64>,
    pub sealed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum I39ExecutionModeDto {
    ExactScan {
        source_may_match: bool,
    },
    FilteredGraph {
        required_fallback: FallbackReasonDto,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I39ExpectedCase {
    pub key: QuerySourceKey,
    pub mode: I39ExecutionModeDto,
    pub row_count: u64,
    pub filter_cardinality: u64,
    pub allow_list_threshold: u64,
    pub rows_examined: u64,
    pub allowed_rows_examined: u64,
    pub vectors_scored: u64,
    pub graph_nodes_visited: u64,
    pub exact_fallback_rows_examined: u64,
    pub returned_candidates: u64,
    pub ef_effective: Option<u64>,
    pub visited_budget: Option<u64>,
    pub sealed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I39Observed {
    pub reports: Vec<BranchReportDto>,
    pub diagnostics_reports: Vec<BranchReportDto>,
    pub receipts: Vec<ExecutionReceiptDto>,
    pub allow_list_threshold: u64,
}

pub fn compare_i39_expected(
    expected: &[I39ExpectedCase],
    observed: &I39Observed,
) -> Result<(), String> {
    let expected = keyed_expected_cases(expected)?;
    let reports = keyed_reports("public report", &observed.reports)?;
    let diagnostics = keyed_reports("diagnostics report", &observed.diagnostics_reports)?;
    let receipts = keyed_receipts(&observed.receipts)?;
    for (kind, observed_count) in [
        ("public report", reports.len()),
        ("diagnostics report", diagnostics.len()),
        ("production execution receipt", receipts.len()),
    ] {
        if observed_count != expected.len() {
            return Err(format!(
                "{I39_CHECKER_ID}: expected-case/{kind} count mismatch expected={} observed={observed_count}",
                expected.len()
            ));
        }
    }
    for (key, expected_case) in &expected {
        let report = reports.get(key).ok_or_else(|| {
            format!(
                "{I39_CHECKER_ID}: missing public report query={} source={}",
                key.query_id, key.source
            )
        })?;
        let diagnostics_report = diagnostics.get(key).ok_or_else(|| {
            format!(
                "{I39_CHECKER_ID}: missing diagnostics report query={} source={}",
                key.query_id, key.source
            )
        })?;
        let receipt = receipts.get(key).ok_or_else(|| {
            format!(
                "{I39_CHECKER_ID}: missing production execution receipt query={} source={}",
                key.query_id, key.source
            )
        })?;
        if *report != *diagnostics_report {
            return Err(format!(
                "{I39_CHECKER_ID} query={} source={} public={:?} diagnostics={:?}",
                key.query_id, key.source, report.branch, diagnostics_report.branch
            ));
        }
        let (expected_branch, expected_fallback) = expected_branch(expected_case);
        if report.branch != expected_branch
            || report.fallback != expected_fallback
            || report.filter_cardinality != expected_case.filter_cardinality
        {
            return Err(format!(
                "{I39_CHECKER_ID} query={} source={} expected_branch={expected_branch:?} observed_branch={:?} expected_fallback={expected_fallback:?} observed_fallback={:?} expected_cardinality={} observed_cardinality={}",
                key.query_id,
                key.source,
                report.branch,
                report.fallback,
                expected_case.filter_cardinality,
                report.filter_cardinality
            ));
        }
        if receipt.branch != expected_branch || receipt.fallback != expected_fallback {
            return Err(format!(
                "{I39_CHECKER_ID} query={} source={} expected_branch={expected_branch:?} executed={:?} expected_fallback={expected_fallback:?} executed_fallback={:?}",
                key.query_id, key.source, receipt.branch, receipt.fallback
            ));
        }
        if observed.allow_list_threshold != expected_case.allow_list_threshold {
            return Err(format!(
                "{I39_CHECKER_ID} query={} source={} expected threshold={} observed threshold={}",
                key.query_id,
                key.source,
                expected_case.allow_list_threshold,
                observed.allow_list_threshold
            ));
        }
        if receipt.row_count != expected_case.row_count
            || receipt.filter_cardinality != expected_case.filter_cardinality
        {
            return Err(format!(
                "{I39_CHECKER_ID} query={} source={} expected row_count/cardinality={}/{} observed={}/{}",
                key.query_id,
                key.source,
                expected_case.row_count,
                expected_case.filter_cardinality,
                receipt.row_count,
                receipt.filter_cardinality
            ));
        }
        for (field, expected_value, observed_value) in [
            (
                "rows_examined",
                expected_case.rows_examined,
                receipt.rows_examined,
            ),
            (
                "allowed_rows_examined",
                expected_case.allowed_rows_examined,
                receipt.allowed_rows_examined,
            ),
            (
                "vectors_scored",
                expected_case.vectors_scored,
                receipt.vectors_scored,
            ),
            (
                "graph_nodes_visited",
                expected_case.graph_nodes_visited,
                receipt.graph_nodes_visited,
            ),
            (
                "exact_fallback_rows_examined",
                expected_case.exact_fallback_rows_examined,
                receipt.exact_fallback_rows_examined,
            ),
            (
                "returned_candidates",
                expected_case.returned_candidates,
                receipt.returned_candidates,
            ),
        ] {
            if observed_value != expected_value {
                return Err(format!(
                    "{I39_CHECKER_ID} query={} source={} expected {field}={expected_value} observed {field}={observed_value}",
                    key.query_id, key.source
                ));
            }
        }
        if receipt.ef_effective != expected_case.ef_effective
            || receipt.visited_budget != expected_case.visited_budget
        {
            return Err(format!(
                "{I39_CHECKER_ID} query={} source={} expected ef/budget={:?}/{:?} observed={:?}/{:?}",
                key.query_id,
                key.source,
                expected_case.ef_effective,
                expected_case.visited_budget,
                receipt.ef_effective,
                receipt.visited_budget
            ));
        }
        if receipt.sealed != expected_case.sealed {
            return Err(format!(
                "{I39_CHECKER_ID} query={} source={} expected sealed={} observed sealed={}",
                key.query_id, key.source, expected_case.sealed, receipt.sealed
            ));
        }
        validate_receipt_semantics(receipt, expected_case.allow_list_threshold)?;
    }
    Ok(())
}

fn keyed_expected_cases(
    expected: &[I39ExpectedCase],
) -> Result<BTreeMap<QuerySourceKey, &I39ExpectedCase>, String> {
    let mut keyed = BTreeMap::new();
    for expected_case in expected {
        if keyed
            .insert(expected_case.key.clone(), expected_case)
            .is_some()
        {
            return Err(format!(
                "{I39_CHECKER_ID}: duplicate expected case query={} source={}",
                expected_case.key.query_id, expected_case.key.source
            ));
        }
    }
    Ok(keyed)
}

fn expected_branch(expected: &I39ExpectedCase) -> (ExecutionBranchDto, FallbackReasonDto) {
    match expected.mode {
        I39ExecutionModeDto::ExactScan {
            source_may_match: false,
        } => (ExecutionBranchDto::Pruned, FallbackReasonDto::None),
        I39ExecutionModeDto::ExactScan {
            source_may_match: true,
        } if expected.filter_cardinality <= expected.allow_list_threshold => {
            (ExecutionBranchDto::ExactAllowList, FallbackReasonDto::None)
        }
        I39ExecutionModeDto::ExactScan {
            source_may_match: true,
        } => (ExecutionBranchDto::MaskedScan, FallbackReasonDto::None),
        I39ExecutionModeDto::FilteredGraph {
            required_fallback: FallbackReasonDto::None | FallbackReasonDto::EfWidened,
        } => (ExecutionBranchDto::FilteredGraph, expected.mode.fallback()),
        I39ExecutionModeDto::FilteredGraph { required_fallback } => {
            (ExecutionBranchDto::GraphExactFallback, required_fallback)
        }
    }
}

impl I39ExecutionModeDto {
    const fn fallback(self) -> FallbackReasonDto {
        match self {
            Self::ExactScan { .. } => FallbackReasonDto::None,
            Self::FilteredGraph { required_fallback } => required_fallback,
        }
    }
}

#[cfg(test)]
pub fn compare_i39(observed: &I39Observed) -> Result<(), String> {
    let reports = keyed_reports("public report", &observed.reports)?;
    let diagnostics = keyed_reports("diagnostics report", &observed.diagnostics_reports)?;
    let receipts = keyed_receipts(&observed.receipts)?;

    if reports.len() != diagnostics.len() {
        return Err(format!(
            "{I39_CHECKER_ID}: public/diagnostics report count mismatch public={} diagnostics={}",
            reports.len(),
            diagnostics.len()
        ));
    }
    for (key, report) in &reports {
        let diagnostics_report = diagnostics.get(key).ok_or_else(|| {
            format!(
                "{I39_CHECKER_ID}: missing diagnostics report query={} source={}",
                key.query_id, key.source
            )
        })?;
        if *report != *diagnostics_report {
            return Err(format!(
                "{I39_CHECKER_ID} query={} source={} public={:?} diagnostics={:?}",
                key.query_id, key.source, report.branch, diagnostics_report.branch
            ));
        }
        let receipt = receipts.get(key).ok_or_else(|| {
            format!(
                "{I39_CHECKER_ID}: missing production execution receipt query={} source={}",
                key.query_id, key.source
            )
        })?;
        if report.branch != receipt.branch {
            return Err(format!(
                "{I39_CHECKER_ID} query={} source={} reported={:?} executed={:?}",
                key.query_id, key.source, report.branch, receipt.branch
            ));
        }
        if report.fallback != receipt.fallback
            || report.filter_cardinality != receipt.filter_cardinality
        {
            return Err(format!(
                "{I39_CHECKER_ID} query={} source={} report_fallback={:?} executed_fallback={:?} report_cardinality={} executed_cardinality={}",
                key.query_id,
                key.source,
                report.fallback,
                receipt.fallback,
                report.filter_cardinality,
                receipt.filter_cardinality
            ));
        }
        validate_receipt_semantics(receipt, observed.allow_list_threshold)?;
    }
    if let Some((key, _)) = receipts.iter().find(|(key, _)| !reports.contains_key(*key)) {
        return Err(format!(
            "{I39_CHECKER_ID}: orphan production execution receipt query={} source={}",
            key.query_id, key.source
        ));
    }
    Ok(())
}

fn keyed_reports<'a>(
    kind: &str,
    reports: &'a [BranchReportDto],
) -> Result<BTreeMap<QuerySourceKey, &'a BranchReportDto>, String> {
    let mut keyed = BTreeMap::new();
    for report in reports {
        if keyed.insert(report.key.clone(), report).is_some() {
            return Err(format!(
                "{I39_CHECKER_ID}: duplicate {kind} query={} source={}",
                report.key.query_id, report.key.source
            ));
        }
    }
    Ok(keyed)
}

fn keyed_receipts(
    receipts: &[ExecutionReceiptDto],
) -> Result<BTreeMap<QuerySourceKey, &ExecutionReceiptDto>, String> {
    let mut keyed = BTreeMap::new();
    for receipt in receipts {
        if keyed.insert(receipt.key.clone(), receipt).is_some() {
            return Err(format!(
                "{I39_CHECKER_ID}: duplicate production execution receipt query={} source={}",
                receipt.key.query_id, receipt.key.source
            ));
        }
    }
    Ok(keyed)
}

fn validate_receipt_semantics(receipt: &ExecutionReceiptDto, threshold: u64) -> Result<(), String> {
    let fail = |detail: &str| {
        Err(format!(
            "{I39_CHECKER_ID} query={} source={} branch={:?} semantic={detail}",
            receipt.key.query_id, receipt.key.source, receipt.branch
        ))
    };
    if receipt.returned_candidates > receipt.filter_cardinality {
        return fail("returned candidates exceed filter cardinality");
    }
    match receipt.branch {
        ExecutionBranchDto::Pruned => {
            if receipt.rows_examined != 0
                || receipt.allowed_rows_examined != 0
                || receipt.vectors_scored != 0
                || receipt.graph_nodes_visited != 0
                || receipt.exact_fallback_rows_examined != 0
                || receipt.returned_candidates != 0
                || receipt.fallback != FallbackReasonDto::None
            {
                return fail("pruned branch recorded execution work");
            }
        }
        ExecutionBranchDto::ExactAllowList => {
            if receipt.filter_cardinality > threshold {
                return fail("allow-list cardinality exceeds threshold");
            }
            if receipt.rows_examined != receipt.allowed_rows_examined
                || receipt.allowed_rows_examined > receipt.filter_cardinality
                || receipt.vectors_scored > receipt.allowed_rows_examined
                || receipt.graph_nodes_visited != 0
                || receipt.exact_fallback_rows_examined != 0
                || receipt.fallback != FallbackReasonDto::None
            {
                return fail("allow-list branch examined work outside allowed rows");
            }
        }
        ExecutionBranchDto::MaskedScan => {
            if receipt.filter_cardinality <= threshold {
                return fail("masked-scan cardinality does not exceed threshold");
            }
            if receipt.rows_examined != receipt.row_count
                || (receipt.row_count != 0 && receipt.rows_examined == 0)
                || receipt.allowed_rows_examined != receipt.filter_cardinality
                || receipt.vectors_scored > receipt.filter_cardinality
                || receipt.graph_nodes_visited != 0
                || receipt.exact_fallback_rows_examined != 0
                || receipt.fallback != FallbackReasonDto::None
            {
                return fail("masked scan counters do not describe a full row sweep");
            }
        }
        ExecutionBranchDto::FilteredGraph => {
            if receipt.graph_nodes_visited == 0
                || receipt.exact_fallback_rows_examined != 0
                || !matches!(
                    receipt.fallback,
                    FallbackReasonDto::None | FallbackReasonDto::EfWidened
                )
            {
                return fail("filtered graph lacks graph work or recorded exact fallback work");
            }
        }
        ExecutionBranchDto::GraphExactFallback => {
            if receipt.graph_nodes_visited == 0
                || receipt.exact_fallback_rows_examined == 0
                || !matches!(
                    receipt.fallback,
                    FallbackReasonDto::VisitedBudget | FallbackReasonDto::CandidateShortfall
                )
            {
                return fail("graph fallback lacks reason, graph attempt, or exact work");
            }
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceMetadataRowDto {
    pub source: String,
    pub row_id: u32,
    pub document_id: u128,
    pub cells: BTreeMap<u32, ScalarCell>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExactHitDto {
    pub source: String,
    pub row_id: u32,
    pub document_id: u128,
    pub distance_bits: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceRangeDto {
    Unstamped,
    Empty,
    Bounded { min: i64, max: i64 },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I38SourceDto {
    pub source: String,
    pub sealed: bool,
    pub range: SourceRangeDto,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I38Input {
    pub sources: Vec<I38SourceDto>,
    pub rows: Vec<SourceMetadataRowDto>,
    pub live: BTreeSet<(String, u32)>,
    pub predicate: PredicateDto,
    pub unfiltered_exact: Vec<ExactHitDto>,
    pub expected_delete_records: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I38Observed {
    pub filtered_exact: Vec<ExactHitDto>,
    pub pruned_sources: BTreeSet<String>,
    pub reports: Vec<BranchReportDto>,
    pub execution_receipts: Vec<ExecutionReceiptDto>,
    pub allow_list_threshold: u64,
    pub wal_delete_records: u64,
}

pub fn expected_i38(input: &I38Input) -> Result<Vec<ExactHitDto>, String> {
    let rows = input
        .rows
        .iter()
        .map(|row| ((row.source.clone(), row.row_id), row))
        .collect::<BTreeMap<_, _>>();
    if rows.len() != input.rows.len() {
        return Err(format!("{I38_CHECKER_ID} duplicate source/row input"));
    }
    let baseline_keys = input
        .unfiltered_exact
        .iter()
        .map(|hit| (hit.source.clone(), hit.row_id))
        .collect::<BTreeSet<_>>();
    if baseline_keys.len() != input.unfiltered_exact.len() {
        return Err(format!("{I38_CHECKER_ID} duplicate unfiltered exact hit"));
    }
    if baseline_keys != input.live {
        let missing = input.live.difference(&baseline_keys).next();
        let extra = baseline_keys.difference(&input.live).next();
        return Err(format!(
            "{I38_CHECKER_ID} unfiltered baseline is not the complete live set missing={missing:?} extra={extra:?}"
        ));
    }
    let mut expected = Vec::new();
    for hit in &input.unfiltered_exact {
        let key = (hit.source.clone(), hit.row_id);
        let row = rows.get(&key).ok_or_else(|| {
            format!(
                "{I38_CHECKER_ID} missing metadata source={} row={}",
                hit.source, hit.row_id
            )
        })?;
        if matches_predicate_row(&input.predicate, &row.cells) {
            expected.push(hit.clone());
        }
    }
    Ok(expected)
}

pub fn compare_i38(input: &I38Input, observed: &I38Observed) -> Result<(), String> {
    let expected = expected_i38(input)?;
    let mut failures = Vec::new();
    let source_states = input
        .sources
        .iter()
        .map(|source| (source.source.clone(), source))
        .collect::<BTreeMap<_, _>>();
    if source_states.len() != input.sources.len() {
        failures.push(format!("{I38_CHECKER_ID} duplicate source lifecycle fact"));
    }
    for row in &input.rows {
        if !source_states.contains_key(&row.source) {
            failures.push(format!(
                "{I38_CHECKER_ID} source={} row={} lacks lifecycle fact",
                row.source, row.row_id
            ));
        }
    }
    for source in &input.sources {
        if let SourceRangeDto::Bounded { min, max } = source.range
            && min > max
        {
            failures.push(format!(
                "{I38_CHECKER_ID} source={} invalid bounds {min}..={max}",
                source.source
            ));
        }
        if source.range == SourceRangeDto::Empty
            && input
                .live
                .iter()
                .any(|(live_source, _)| live_source == &source.source)
        {
            failures.push(format!(
                "{I38_CHECKER_ID} source={} Empty range retains a live row",
                source.source
            ));
        }
    }
    let required_sources = source_states.keys().cloned().collect::<BTreeSet<_>>();
    let reports = keyed_reports("I38 public report", &observed.reports)?;
    let receipts = keyed_receipts(&observed.execution_receipts)?;
    for source in &required_sources {
        let matching_reports = reports
            .iter()
            .filter(|(key, _)| key.source == *source)
            .collect::<Vec<_>>();
        if matching_reports.is_empty() {
            failures.push(format!(
                "{I38_CHECKER_ID} source={source} missing public branch report"
            ));
            continue;
        }
        if matching_reports.len() != 1 {
            failures.push(format!(
                "{I38_CHECKER_ID} source={source} expected exactly one public branch report observed={}",
                matching_reports.len()
            ));
            continue;
        }
        let (key, report) = matching_reports[0];
        let Some(receipt) = receipts.get(key) else {
            failures.push(format!(
                "{I38_CHECKER_ID} source={source} missing production execution receipt"
            ));
            continue;
        };
        if report.branch != receipt.branch
            || report.fallback != receipt.fallback
            || report.filter_cardinality != receipt.filter_cardinality
        {
            failures.push(format!(
                "{I38_CHECKER_ID} source={source} report/receipt mismatch report={:?}/{:?}/{} receipt={:?}/{:?}/{}",
                report.branch,
                report.fallback,
                report.filter_cardinality,
                receipt.branch,
                receipt.fallback,
                receipt.filter_cardinality
            ));
        }
        let expected_sealed = source_states.get(source).is_some_and(|state| state.sealed);
        if receipt.sealed != expected_sealed {
            failures.push(format!(
                "{I38_CHECKER_ID} source={source} receipt sealed={} disagrees with source lifecycle",
                receipt.sealed
            ));
        }
        if let Err(error) = validate_receipt_semantics(receipt, observed.allow_list_threshold) {
            failures.push(error);
        }
    }
    for key in reports.keys() {
        if !required_sources.contains(&key.source) {
            failures.push(format!(
                "{I38_CHECKER_ID} source={} orphan public branch report",
                key.source
            ));
        }
    }
    for key in receipts.keys() {
        if !reports.contains_key(key) {
            failures.push(format!(
                "{I38_CHECKER_ID} source={} orphan production execution receipt",
                key.source
            ));
        }
    }
    let reported_pruned = observed
        .reports
        .iter()
        .filter(|report| report.branch == ExecutionBranchDto::Pruned)
        .map(|report| report.key.source.clone())
        .collect::<BTreeSet<_>>();
    if reported_pruned != observed.pruned_sources {
        failures.push(format!(
            "{I38_CHECKER_ID} pruned source ledger mismatch reports={reported_pruned:?} observed={:?}",
            observed.pruned_sources
        ));
    }
    if observed.wal_delete_records != input.expected_delete_records {
        failures.push(format!(
            "{I38_CHECKER_ID} public delete WAL records expected={} observed={}",
            input.expected_delete_records, observed.wal_delete_records
        ));
    }
    for source in &observed.pruned_sources {
        if let Some(row) = input.rows.iter().find(|row| {
            row.source == *source
                && input.live.contains(&(row.source.clone(), row.row_id))
                && matches_predicate_row(&input.predicate, &row.cells)
        }) {
            failures.push(format!(
                "{I38_CHECKER_ID} source={source} unsound_prune matching_row={}",
                row.row_id
            ));
        }
        let matching_receipts = observed
            .execution_receipts
            .iter()
            .filter(|receipt| receipt.key.source == *source)
            .collect::<Vec<_>>();
        if matching_receipts.len() != 1
            || matching_receipts
                .first()
                .is_none_or(|receipt| receipt.branch != ExecutionBranchDto::Pruned)
        {
            failures.push(format!(
                "{I38_CHECKER_ID} source={source} expected exactly one Pruned execution receipt observed={}",
                matching_receipts.len()
            ));
        }
    }
    for receipt in &observed.execution_receipts {
        if receipt.branch == ExecutionBranchDto::Pruned
            && !observed.pruned_sources.contains(&receipt.key.source)
        {
            failures.push(format!(
                "{I38_CHECKER_ID} source={} orphan Pruned execution receipt",
                receipt.key.source
            ));
        }
    }
    if expected != observed.filtered_exact {
        if let Some(hit) = expected
            .iter()
            .find(|hit| !observed.filtered_exact.contains(hit))
        {
            failures.push(format!(
                "missing_exact_hit document={} distance_bits={}",
                hit.document_id, hit.distance_bits
            ));
        } else if let Some(hit) = observed
            .filtered_exact
            .iter()
            .find(|hit| !expected.contains(hit))
        {
            failures.push(format!(
                "extra_exact_hit document={} distance_bits={}",
                hit.document_id, hit.distance_bits
            ));
        } else {
            let position = expected
                .iter()
                .zip(&observed.filtered_exact)
                .position(|(left, right)| left != right)
                .unwrap_or_default();
            failures.push(format!("reordered_exact_hit position={position}"));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

fn matches_predicate_row(predicate: &PredicateDto, cells: &BTreeMap<u32, ScalarCell>) -> bool {
    match predicate {
        PredicateDto::Eq { column, value } => cells
            .get(column)
            .is_some_and(|stored| scalar_equal(stored, value)),
        PredicateDto::In { column, values } => cells
            .get(column)
            .is_some_and(|stored| values.iter().any(|value| scalar_equal(stored, value))),
        PredicateDto::Range {
            column,
            lower,
            upper,
        } => cells
            .get(column)
            .is_some_and(|stored| scalar_in_range(stored, lower.as_ref(), upper.as_ref())),
        PredicateDto::Exists(column) => cells
            .get(column)
            .is_some_and(|cell| *cell != ScalarCell::Null),
        PredicateDto::IsNull(column) => cells
            .get(column)
            .is_none_or(|cell| *cell == ScalarCell::Null),
        PredicateDto::And(children) => children
            .iter()
            .all(|child| matches_predicate_row(child, cells)),
        PredicateDto::Or(children) => children
            .iter()
            .any(|child| matches_predicate_row(child, cells)),
        PredicateDto::Not(child) => !matches_predicate_row(child, cells),
    }
}

impl<'a> CheckedCursor<'a> {
    const fn new(artifact: &'static str, bytes: &'a [u8]) -> Self {
        Self {
            artifact,
            bytes,
            position: 0,
        }
    }

    const fn position(&self) -> usize {
        self.position
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], String> {
        let end = self
            .position
            .checked_add(length)
            .ok_or_else(|| format!("{} offset overflow", self.artifact))?;
        let bytes = self.bytes.get(self.position..end).ok_or_else(|| {
            format!(
                "{} truncated at {} for {} bytes",
                self.artifact, self.position, length
            )
        })?;
        self.position = end;
        Ok(bytes)
    }

    fn u8(&mut self) -> Result<u8, String> {
        self.take(1)?
            .first()
            .copied()
            .ok_or_else(|| format!("{} missing u8", self.artifact))
    }

    fn u16(&mut self) -> Result<u16, String> {
        let bytes: [u8; 2] = self
            .take(2)?
            .try_into()
            .map_err(|_| format!("{} invalid u16", self.artifact))?;
        Ok(u16::from_le_bytes(bytes))
    }

    fn u32(&mut self) -> Result<u32, String> {
        let bytes: [u8; 4] = self
            .take(4)?
            .try_into()
            .map_err(|_| format!("{} invalid u32", self.artifact))?;
        Ok(u32::from_le_bytes(bytes))
    }

    fn u64(&mut self) -> Result<u64, String> {
        let bytes: [u8; 8] = self
            .take(8)?
            .try_into()
            .map_err(|_| format!("{} invalid u64", self.artifact))?;
        Ok(u64::from_le_bytes(bytes))
    }

    fn i64(&mut self) -> Result<i64, String> {
        let bytes: [u8; 8] = self
            .take(8)?
            .try_into()
            .map_err(|_| format!("{} invalid i64", self.artifact))?;
        Ok(i64::from_le_bytes(bytes))
    }

    fn usize_from_u32(&mut self) -> Result<usize, String> {
        usize::try_from(self.u32()?).map_err(|_| format!("{} u32 exceeds usize", self.artifact))
    }

    fn string_bytes(&mut self) -> Result<(Vec<u8>, StringByteSpans), String> {
        let start = self.position;
        let length = self.usize_from_u32()?;
        let length_end = self.position;
        let payload_start = self.position;
        let bytes = self.take(length)?.to_vec();
        std::str::from_utf8(&bytes).map_err(|error| format!("{} UTF-8: {error}", self.artifact))?;
        Ok((
            bytes,
            StringByteSpans {
                full: ByteSpan {
                    start,
                    end: self.position,
                },
                length: ByteSpan {
                    start,
                    end: length_end,
                },
                payload: ByteSpan {
                    start: payload_start,
                    end: self.position,
                },
            },
        ))
    }

    fn finish(self) -> Result<(), String> {
        if self.position == self.bytes.len() {
            Ok(())
        } else {
            Err(format!(
                "{} has {} trailing bytes",
                self.artifact,
                self.bytes.len().saturating_sub(self.position)
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push_definition(bytes: &mut Vec<u8>, id: u32, kind: u16, nullable: u16, name: &[u8]) {
        bytes.extend_from_slice(&id.to_le_bytes());
        bytes.extend_from_slice(&kind.to_le_bytes());
        bytes.extend_from_slice(&nullable.to_le_bytes());
        bytes.extend_from_slice(&(name.len() as u32).to_le_bytes());
        bytes.extend_from_slice(name);
    }

    fn valid_columns_bytes(all_null_dictionary: bool) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&2_u32.to_le_bytes());
        bytes.extend_from_slice(&7_u32.to_le_bytes());
        push_definition(&mut bytes, 0, 2, 0, b"ts");
        push_definition(&mut bytes, 1, 1, 1, b"u");
        push_definition(&mut bytes, 2, 2, 1, b"i");
        push_definition(&mut bytes, 3, 3, 1, b"f");
        push_definition(&mut bytes, 4, 4, 1, b"b");
        push_definition(&mut bytes, 5, 5, 1, b"d");
        push_definition(&mut bytes, 6, 6, 1, b"r");

        bytes.extend_from_slice(&1_u32.to_le_bytes());
        bytes.push(0b11);
        bytes.extend_from_slice(&7_i64.to_le_bytes());
        bytes.extend_from_slice(&8_i64.to_le_bytes());

        for value in [u64::MAX, 0] {
            if value == u64::MAX {
                bytes.extend_from_slice(&1_u32.to_le_bytes());
                bytes.push(0b01);
            }
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes.extend_from_slice(&1_u32.to_le_bytes());
        bytes.push(0b01);
        bytes.extend_from_slice(&i64::MIN.to_le_bytes());
        bytes.extend_from_slice(&0_i64.to_le_bytes());
        bytes.extend_from_slice(&1_u32.to_le_bytes());
        bytes.push(0b01);
        bytes.extend_from_slice(&(-0.0_f64).to_bits().to_le_bytes());
        bytes.extend_from_slice(&0_u64.to_le_bytes());
        bytes.extend_from_slice(&1_u32.to_le_bytes());
        bytes.push(0b01);
        bytes.extend_from_slice(&[1, 0]);

        bytes.extend_from_slice(&1_u32.to_le_bytes());
        bytes.push(if all_null_dictionary { 0 } else { 0b01 });
        bytes.extend_from_slice(&u32::from(!all_null_dictionary).to_le_bytes());
        if !all_null_dictionary {
            bytes.extend_from_slice(&1_u32.to_le_bytes());
            bytes.push(b'x');
        }
        bytes.extend_from_slice(&2_u16.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());

        bytes.extend_from_slice(&1_u32.to_le_bytes());
        bytes.push(0b01);
        bytes.extend_from_slice(&3_u32.to_le_bytes());
        bytes.extend_from_slice(b"raw");
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes
    }

    fn i36_fixture() -> (I36Input, I36Observed) {
        let definitions = vec![ColumnDefinitionDto {
            id: 0,
            name: b"ts".to_vec(),
            kind: ColumnKind::I64,
            nullable: false,
        }];
        let row = BTreeMap::from([(0, ScalarCell::I64(7))]);
        (
            I36Input {
                source: "sealed-7".to_owned(),
                definitions: definitions.clone(),
                rows: vec![row.clone()],
            },
            I36Observed {
                active_rows: vec![row.clone()],
                raw: ParsedColumns {
                    row_count: 1,
                    definitions,
                    row_count_span: ByteSpan { start: 0, end: 4 },
                    column_count_span: ByteSpan { start: 4, end: 8 },
                    definition_spans: BTreeMap::new(),
                    presence_spans: BTreeMap::new(),
                    dictionary_spans: BTreeMap::new(),
                    cells: BTreeMap::from([(
                        (0, 0),
                        ParsedCell {
                            present: true,
                            logical: ScalarCell::I64(7),
                            physical: PhysicalCell::I64(7),
                            span: ByteSpan { start: 0, end: 8 },
                            length_span: None,
                            payload_span: ByteSpan { start: 0, end: 8 },
                        },
                    )]),
                },
                reader_rows: vec![row.clone()],
                public_rows: vec![row],
            },
        )
    }

    #[test]
    fn i36_plant_reports_exact_source_column_row_and_primitives() {
        let (input, mut observed) = i36_fixture();
        observed.raw.cells.get_mut(&(0, 0)).unwrap().logical = ScalarCell::I64(8);
        let error = match compare_i36(&input, &observed) {
            Err(error) => error,
            Ok(()) => panic!(
                "I36.column-roundtrip.v2 source=sealed-7 column=0 row=0 expected=I64(7) observed=I64(8); comparator accepted planted mismatch"
            ),
        };
        assert!(
            error.contains("I36.column-roundtrip.v2 source=sealed-7 column=0 row=0 expected=I64(7) observed=I64(8)"),
            "I36.column-roundtrip.v2 source=sealed-7 column=0 row=0 expected=I64(7) observed=I64(8); comparator returned {error:?}"
        );
        assert_eq!(
            attest_i36(&input, &observed).first_difference,
            Some(FirstDifference {
                checker_id: I36_CHECKER_ID,
                path: "source=sealed-7/column=0/row=0/raw.logical".to_owned(),
                kind: DifferenceKind::PrimitiveMismatch,
                row: Some(0),
                expected: "I64(7)".to_owned(),
                observed: "I64(8)".to_owned(),
            })
        );
    }

    #[test]
    fn i36_clean_observation_is_accepted_before_presence_and_dictionary_plants() {
        let (input, observed) = i36_fixture();
        compare_i36(&input, &observed).expect("clean I36 observation");

        let mut presence = observed.clone();
        presence.raw.cells.get_mut(&(0, 0)).unwrap().present = false;
        assert_eq!(
            compare_i36(&input, &presence).unwrap_err(),
            "I36.column-roundtrip.v2 source=sealed-7 column=0 row=0 expected_present=true observed_present=false"
        );

        let mut dictionary_input = input.clone();
        dictionary_input.definitions.push(ColumnDefinitionDto {
            id: 1,
            name: b"d".to_vec(),
            kind: ColumnKind::DictionaryString,
            nullable: false,
        });
        dictionary_input.rows[0].insert(1, ScalarCell::Utf8(b"expected".to_vec()));
        let mut dictionary = observed;
        dictionary.raw.definitions = dictionary_input.definitions.clone();
        dictionary.active_rows = dictionary_input.rows.clone();
        dictionary.reader_rows = dictionary_input.rows.clone();
        dictionary.public_rows = dictionary_input.rows.clone();
        dictionary.raw.cells.insert(
            (0, 1),
            ParsedCell {
                present: true,
                logical: ScalarCell::Utf8(b"expected".to_vec()),
                physical: PhysicalCell::DictionaryCode {
                    code: 0,
                    decoded: Some(b"observed".to_vec()),
                },
                span: ByteSpan { start: 8, end: 10 },
                length_span: None,
                payload_span: ByteSpan { start: 8, end: 10 },
            },
        );
        assert!(
            compare_i36(&dictionary_input, &dictionary)
                .unwrap_err()
                .contains(
                    "column=1 row=0 expected_physical=Utf8([101, 120, 112, 101, 99, 116, 101, 100])"
                )
        );
    }

    #[test]
    fn canonical_digests_bind_each_owned_input_and_observation_family() {
        let (i36_input, i36_observed) = i36_fixture();
        let mut mutated_i36_input = i36_input.clone();
        mutated_i36_input.rows[0].insert(0, ScalarCell::I64(8));
        assert_ne!(
            canonical_i36_input_digest(&i36_input),
            canonical_i36_input_digest(&mutated_i36_input)
        );
        let mut mutated_i36_observed = i36_observed.clone();
        mutated_i36_observed.raw.row_count_span.end += 1;
        assert_ne!(
            canonical_i36_observed_digest(&i36_observed),
            canonical_i36_observed_digest(&mutated_i36_observed)
        );

        let i37_input = I37Input {
            rows: vec![MetadataRowDto {
                row_id: 0,
                cells: BTreeMap::from([(1, ScalarCell::U64(7))]),
            }],
            live: BTreeSet::from([0]),
            predicate: PredicateDto::Eq {
                column: 1,
                value: ScalarCell::U64(7),
            },
            sources: Vec::new(),
        };
        let mut mutated_i37_input = i37_input.clone();
        mutated_i37_input.predicate = PredicateDto::Exists(1);
        assert_ne!(
            canonical_i37_input_digest(&i37_input),
            canonical_i37_input_digest(&mutated_i37_input)
        );
        let i37_observed = I37Observed {
            evaluator: BTreeSet::from([0]),
            public_results: vec![0],
            sources: Vec::new(),
            allow_list_threshold: 64,
        };
        let mut mutated_i37_observed = i37_observed.clone();
        mutated_i37_observed.public_results.push(0);
        assert_ne!(
            canonical_i37_observed_digest(&i37_observed),
            canonical_i37_observed_digest(&mutated_i37_observed)
        );

        let i38_input = I38Input {
            sources: Vec::new(),
            rows: Vec::new(),
            live: BTreeSet::new(),
            predicate: PredicateDto::And(Vec::new()),
            unfiltered_exact: Vec::new(),
            expected_delete_records: 0,
        };
        let mut mutated_i38_input = i38_input.clone();
        mutated_i38_input.expected_delete_records = 1;
        assert_ne!(
            canonical_i38_input_digest(&i38_input),
            canonical_i38_input_digest(&mutated_i38_input)
        );
        let i38_observed = I38Observed {
            filtered_exact: Vec::new(),
            pruned_sources: BTreeSet::new(),
            reports: Vec::new(),
            execution_receipts: Vec::new(),
            allow_list_threshold: 64,
            wal_delete_records: 0,
        };
        let mut mutated_i38_observed = i38_observed.clone();
        mutated_i38_observed.wal_delete_records = 1;
        assert_ne!(
            canonical_i38_observed_digest(&i38_observed),
            canonical_i38_observed_digest(&mutated_i38_observed)
        );

        let i39_expected = vec![I39ExpectedCase {
            key: QuerySourceKey {
                query_id: 39,
                source: "active".to_owned(),
            },
            mode: I39ExecutionModeDto::ExactScan {
                source_may_match: true,
            },
            row_count: 1,
            filter_cardinality: 1,
            allow_list_threshold: 64,
            rows_examined: 1,
            allowed_rows_examined: 1,
            vectors_scored: 1,
            graph_nodes_visited: 0,
            exact_fallback_rows_examined: 0,
            returned_candidates: 1,
            ef_effective: None,
            visited_budget: None,
            sealed: false,
        }];
        let mut mutated_i39_expected = i39_expected.clone();
        mutated_i39_expected[0].returned_candidates = 0;
        assert_ne!(
            canonical_i39_input_digest(&i39_expected),
            canonical_i39_input_digest(&mutated_i39_expected)
        );
        let i39_observed = I39Observed {
            reports: Vec::new(),
            diagnostics_reports: Vec::new(),
            receipts: Vec::new(),
            allow_list_threshold: 64,
        };
        let mut mutated_i39_observed = i39_observed.clone();
        mutated_i39_observed.allow_list_threshold = 65;
        assert_ne!(
            canonical_i39_observed_digest(&i39_observed),
            canonical_i39_observed_digest(&mutated_i39_observed)
        );
    }

    #[test]
    fn i36_literal_parser_preserves_every_type_null_and_ieee_bits() {
        let parsed = parse_columns(&valid_columns_bytes(false)).expect("valid literal Columns v1");
        assert_eq!(parsed.row_count, 2);
        assert_eq!(parsed.definitions.len(), 7);
        assert_eq!(parsed.cells[&(0, 1)].logical, ScalarCell::U64(u64::MAX));
        assert_eq!(parsed.cells[&(1, 1)].logical, ScalarCell::Null);
        assert_eq!(parsed.cells[&(0, 2)].logical, ScalarCell::I64(i64::MIN));
        assert_eq!(
            parsed.cells[&(0, 3)].logical,
            ScalarCell::F64Bits((-0.0_f64).to_bits())
        );
        assert_eq!(parsed.cells[&(0, 4)].logical, ScalarCell::Bool(true));
        assert_eq!(
            parsed.cells[&(0, 5)].logical,
            ScalarCell::Utf8(b"x".to_vec())
        );
        assert_eq!(parsed.cells[&(1, 5)].logical, ScalarCell::Null);
        assert_eq!(
            parsed.cells[&(0, 6)].logical,
            ScalarCell::Utf8(b"raw".to_vec())
        );
        assert_eq!(parsed.cells[&(1, 6)].logical, ScalarCell::Null);

        let all_null = parse_columns(&valid_columns_bytes(true))
            .expect("an empty dictionary is valid when every code belongs to a null row");
        assert_eq!(all_null.cells[&(0, 5)].logical, ScalarCell::Null);
        assert_eq!(all_null.cells[&(1, 5)].logical, ScalarCell::Null);
        assert!(matches!(
            all_null.cells[&(0, 5)].physical,
            PhysicalCell::DictionaryCode {
                code: 0,
                decoded: None
            }
        ));
    }

    #[test]
    fn i36_literal_parser_exposes_exact_semantic_spans_for_clean_bytes() {
        let bytes = valid_columns_bytes(false);
        let parsed = parse_columns(&bytes).expect("valid literal Columns v1");
        assert_eq!(parsed.row_count_span, ByteSpan { start: 0, end: 4 });
        assert_eq!(parsed.column_count_span, ByteSpan { start: 4, end: 8 });

        let timestamp = parsed
            .definition_spans
            .get(&0)
            .expect("timestamp definition span");
        assert_eq!(
            &bytes[timestamp.id.start..timestamp.id.end],
            &0_u32.to_le_bytes()
        );
        assert_eq!(
            &bytes[timestamp.kind.start..timestamp.kind.end],
            &2_u16.to_le_bytes()
        );
        assert_eq!(
            &bytes[timestamp.name.payload.start..timestamp.name.payload.end],
            b"ts"
        );

        let raw_presence = parsed
            .presence_spans
            .get(&6)
            .expect("raw-string presence span");
        assert_eq!(
            &bytes[raw_presence.bitmap.start..raw_presence.bitmap.end],
            &[0b01]
        );
        let raw = &parsed.cells[&(0, 6)];
        assert_eq!(
            &bytes[raw.length_span.expect("raw length").start..raw.payload_span.start],
            &3_u32.to_le_bytes()
        );
        assert_eq!(&bytes[raw.payload_span.start..raw.payload_span.end], b"raw");

        let dictionary = parsed.dictionary_spans.get(&5).expect("dictionary spans");
        assert_eq!(dictionary.entries.len(), 1);
        assert_eq!(
            &bytes[dictionary.entries[0].payload.start..dictionary.entries[0].payload.end],
            b"x"
        );
        assert_eq!(
            &bytes[dictionary.width.start..dictionary.width.end],
            &2_u16.to_le_bytes()
        );
        assert_eq!(
            parsed.cells[&(0, 5)].span.end - parsed.cells[&(0, 5)].span.start,
            2
        );
    }

    #[test]
    fn i36_literal_alive_parser_rejects_truncation_tail_and_trailing_bytes() {
        let mut valid = Vec::new();
        valid.extend_from_slice(&10_u32.to_le_bytes());
        valid.extend_from_slice(&2_u32.to_le_bytes());
        valid.extend_from_slice(&[0b0101_0101, 0b0000_0001]);
        let parsed = parse_alive(&valid).expect("valid Alive v1");
        assert_eq!(parsed.live, BTreeSet::from([0, 2, 4, 6, 8]));

        let truncated = &valid[..valid.len() - 1];
        assert!(parse_alive(truncated).unwrap_err().contains("truncated"));
        let mut nonzero_tail = valid.clone();
        *nonzero_tail.last_mut().unwrap() = 0b1000_0001;
        assert_eq!(
            parse_alive(&nonzero_tail).unwrap_err(),
            "alive non-zero bitmap tail padding"
        );
        let mut trailing = valid;
        trailing.push(0);
        assert!(
            parse_alive(&trailing)
                .unwrap_err()
                .contains("trailing bytes")
        );
    }

    #[test]
    fn i36_independent_malformed_catalog_covers_required_layout_classes() {
        let clean_columns = valid_columns_bytes(false);
        let parsed = parse_columns(&clean_columns).expect("independent clean Columns fixture");
        let definition_cutoff = parsed
            .definition_spans
            .get(&1)
            .expect("U64 definition spans")
            .nullable
            .start
            + 1;
        let dictionary_length_cutoff = parsed
            .dictionary_spans
            .get(&5)
            .and_then(|spans| spans.entries.first())
            .expect("dictionary entry spans")
            .length
            .start
            + 2;
        let raw_length_cutoff = parsed
            .cells
            .get(&(0, 6))
            .and_then(|cell| cell.length_span)
            .expect("raw-string length span")
            .start
            + 2;

        let mut row_mismatch = Vec::new();
        row_mismatch.extend_from_slice(&1_u32.to_le_bytes());
        row_mismatch.extend_from_slice(&1_u32.to_le_bytes());
        push_definition(&mut row_mismatch, 0, 2, 0, b"ts");
        row_mismatch.extend_from_slice(&1_u32.to_le_bytes());
        row_mismatch.push(0b1);
        row_mismatch.extend_from_slice(&7_i64.to_le_bytes());
        let mut alive_header = Vec::new();
        alive_header.extend_from_slice(&2_u32.to_le_bytes()[..3]);
        let mut alive_length = Vec::new();
        alive_length.extend_from_slice(&2_u32.to_le_bytes());
        alive_length.extend_from_slice(&0_u32.to_le_bytes());
        let mut alive_row_mismatch = Vec::new();
        alive_row_mismatch.extend_from_slice(&1_u32.to_le_bytes());
        alive_row_mismatch.extend_from_slice(&1_u32.to_le_bytes());
        alive_row_mismatch.push(0b1);

        let cases = vec![
            (
                "columns-definition-truncation",
                parse_columns(&clean_columns[..definition_cutoff]).map(|_| ()),
                "columns truncated",
            ),
            (
                "dictionary-length-truncation",
                parse_columns(&clean_columns[..dictionary_length_cutoff]).map(|_| ()),
                "columns truncated",
            ),
            (
                "raw-string-length-truncation",
                parse_columns(&clean_columns[..raw_length_cutoff]).map(|_| ()),
                "columns truncated",
            ),
            (
                "columns-row-count-mismatch",
                parse_columns(&row_mismatch).and_then(|columns| {
                    (columns.row_count == 2).then_some(()).ok_or_else(|| {
                        format!("columns row count {}, expected 2", columns.row_count)
                    })
                }),
                "columns row count 1, expected 2",
            ),
            (
                "alive-header-truncation",
                parse_alive(&alive_header).map(|_| ()),
                "alive truncated",
            ),
            (
                "alive-declared-length-mismatch",
                parse_alive(&alive_length).map(|_| ()),
                "alive bitmap length 0, expected 1",
            ),
            (
                "alive-row-count-mismatch",
                parse_alive(&alive_row_mismatch).and_then(|alive| {
                    (alive.row_count == 2)
                        .then_some(())
                        .ok_or_else(|| format!("alive row count {}, expected 2", alive.row_count))
                }),
                "alive row count 1, expected 2",
            ),
        ];
        let mut covered = Vec::new();
        for (label, result, expected) in cases {
            let error = result.expect_err(label);
            assert!(
                error.contains(expected),
                "{label}: expected {expected:?}, observed {error:?}"
            );
            covered.push(label);
        }
        assert_eq!(
            covered,
            [
                "columns-definition-truncation",
                "dictionary-length-truncation",
                "raw-string-length-truncation",
                "columns-row-count-mismatch",
                "alive-header-truncation",
                "alive-declared-length-mismatch",
                "alive-row-count-mismatch",
            ]
        );
    }

    #[test]
    fn i37_plant_reports_operator_path_and_extra_row() {
        let input = I37Input {
            rows: vec![
                MetadataRowDto {
                    row_id: 0,
                    cells: BTreeMap::from([(1, ScalarCell::I64(7))]),
                },
                MetadataRowDto {
                    row_id: 1,
                    cells: BTreeMap::from([(1, ScalarCell::I64(9))]),
                },
            ],
            live: BTreeSet::from([0, 1]),
            predicate: PredicateDto::Eq {
                column: 1,
                value: ScalarCell::I64(7),
            },
            sources: Vec::new(),
        };
        let observed = I37Observed {
            evaluator: BTreeSet::from([0, 1]),
            public_results: vec![0],
            sources: Vec::new(),
            allow_list_threshold: 64,
        };
        let error = match compare_i37(&input, &observed) {
            Err(error) => error,
            Ok(()) => panic!(
                "I37.bitmap-algebra.v2 path=root/Eq(column=1) extra_row=1; comparator accepted planted mismatch"
            ),
        };
        assert!(
            error.contains("I37.bitmap-algebra.v2 path=root/Eq(column=1) extra_row=1"),
            "unexpected comparator failure: {error}"
        );
        let attestation = attest_i37(&input, &observed);
        assert_ne!(
            attestation.input_digest, attestation.observed_digest,
            "canonical primitive input and mismatched observation digests alias"
        );
        assert_eq!(
            attestation.first_difference,
            Some(FirstDifference {
                checker_id: I37_CHECKER_ID,
                path: "root/Eq(column=1)".to_owned(),
                kind: DifferenceKind::ExtraRow,
                row: Some(1),
                expected: "absent".to_owned(),
                observed: "present".to_owned(),
            })
        );
    }

    #[test]
    fn i37_clean_observation_is_accepted_before_set_plant() {
        let input = I37Input {
            rows: vec![MetadataRowDto {
                row_id: 0,
                cells: BTreeMap::from([(1, ScalarCell::I64(7))]),
            }],
            live: BTreeSet::from([0]),
            predicate: PredicateDto::Eq {
                column: 1,
                value: ScalarCell::I64(7),
            },
            sources: Vec::new(),
        };
        compare_i37(
            &input,
            &I37Observed {
                evaluator: BTreeSet::from([0]),
                public_results: vec![0],
                sources: Vec::new(),
                allow_list_threshold: 64,
            },
        )
        .expect("clean I37 observation");
    }

    #[test]
    fn i37_matrix_uses_literal_btreeset_algebra_and_alive_bounded_not() {
        let rows = vec![
            MetadataRowDto {
                row_id: 0,
                cells: BTreeMap::from([
                    (1, ScalarCell::U64(1)),
                    (2, ScalarCell::I64(-1)),
                    (3, ScalarCell::F64Bits((-0.0_f64).to_bits())),
                    (4, ScalarCell::Bool(true)),
                    (5, ScalarCell::Utf8(b"a".to_vec())),
                    (6, ScalarCell::Utf8(b"raw".to_vec())),
                ]),
            },
            MetadataRowDto {
                row_id: 1,
                cells: BTreeMap::from([
                    (1, ScalarCell::U64(2)),
                    (2, ScalarCell::I64(0)),
                    (3, ScalarCell::F64Bits(f64::NAN.to_bits())),
                    (4, ScalarCell::Bool(false)),
                    (5, ScalarCell::Utf8(b"b".to_vec())),
                    (6, ScalarCell::Null),
                ]),
            },
            MetadataRowDto {
                row_id: 2,
                cells: BTreeMap::from([
                    (1, ScalarCell::U64(3)),
                    (2, ScalarCell::I64(1)),
                    (3, ScalarCell::F64Bits(1.0_f64.to_bits())),
                    (4, ScalarCell::Bool(true)),
                    (5, ScalarCell::Utf8(b"a".to_vec())),
                    (6, ScalarCell::Utf8(Vec::new())),
                ]),
            },
        ];
        let live = BTreeSet::from([0, 2]);
        let cases = [
            (
                PredicateDto::Eq {
                    column: 1,
                    value: ScalarCell::U64(1),
                },
                BTreeSet::from([0]),
            ),
            (
                PredicateDto::In {
                    column: 5,
                    values: vec![
                        ScalarCell::Utf8(b"a".to_vec()),
                        ScalarCell::Utf8(b"a".to_vec()),
                    ],
                },
                BTreeSet::from([0, 2]),
            ),
            (
                PredicateDto::Range {
                    column: 2,
                    lower: Some(RangeBoundDto {
                        value: ScalarCell::I64(-1),
                        inclusive: false,
                    }),
                    upper: Some(RangeBoundDto {
                        value: ScalarCell::I64(1),
                        inclusive: true,
                    }),
                },
                BTreeSet::from([2]),
            ),
            (
                PredicateDto::Range {
                    column: 3,
                    lower: None,
                    upper: None,
                },
                BTreeSet::from([0, 2]),
            ),
            (PredicateDto::IsNull(6), BTreeSet::new()),
            (PredicateDto::Exists(6), BTreeSet::from([0, 2])),
            (PredicateDto::And(Vec::new()), BTreeSet::from([0, 2])),
            (PredicateDto::Or(Vec::new()), BTreeSet::new()),
            (
                PredicateDto::Not(Box::new(PredicateDto::Not(Box::new(PredicateDto::Eq {
                    column: 4,
                    value: ScalarCell::Bool(true),
                })))),
                BTreeSet::from([0, 2]),
            ),
        ];
        for (predicate, expected) in cases {
            let result = evaluate_i37(&I37Input {
                rows: rows.clone(),
                live: live.clone(),
                predicate,
                sources: Vec::new(),
            })
            .expect("typed predicate");
            assert_eq!(result.result, expected);
        }
    }

    #[test]
    fn i39_plant_reports_exact_query_source_branch_mismatch() {
        let key = QuerySourceKey {
            query_id: 39,
            source: "sealed-9".to_owned(),
        };
        let observed = I39Observed {
            reports: vec![BranchReportDto {
                key: key.clone(),
                branch: ExecutionBranchDto::MaskedScan,
                fallback: FallbackReasonDto::None,
                filter_cardinality: 65,
            }],
            diagnostics_reports: vec![BranchReportDto {
                key: key.clone(),
                branch: ExecutionBranchDto::MaskedScan,
                fallback: FallbackReasonDto::None,
                filter_cardinality: 65,
            }],
            receipts: vec![ExecutionReceiptDto {
                key,
                branch: ExecutionBranchDto::ExactAllowList,
                fallback: FallbackReasonDto::None,
                row_count: 65,
                filter_cardinality: 65,
                rows_examined: 65,
                allowed_rows_examined: 65,
                vectors_scored: 65,
                graph_nodes_visited: 0,
                exact_fallback_rows_examined: 0,
                returned_candidates: 10,
                ef_effective: None,
                visited_budget: None,
                sealed: true,
            }],
            allow_list_threshold: 64,
        };
        let error = match compare_i39(&observed) {
            Err(error) => error,
            Ok(()) => panic!(
                "I39.executed-branch.v2 query=39 source=sealed-9 reported=MaskedScan executed=ExactAllowList; comparator accepted planted mismatch"
            ),
        };
        assert!(
            error.contains("I39.executed-branch.v2 query=39 source=sealed-9 reported=MaskedScan executed=ExactAllowList"),
            "unexpected comparator failure: {error}"
        );
        let expected = [I39ExpectedCase {
            key: QuerySourceKey {
                query_id: 39,
                source: "sealed-9".to_owned(),
            },
            mode: I39ExecutionModeDto::ExactScan {
                source_may_match: true,
            },
            row_count: 65,
            filter_cardinality: 65,
            allow_list_threshold: 64,
            rows_examined: 65,
            allowed_rows_examined: 65,
            vectors_scored: 65,
            graph_nodes_visited: 0,
            exact_fallback_rows_examined: 0,
            returned_candidates: 10,
            ef_effective: None,
            visited_budget: None,
            sealed: true,
        }];
        assert_eq!(
            attest_i39(&expected, &observed).first_difference,
            Some(FirstDifference {
                checker_id: I39_CHECKER_ID,
                path: "query=39/source=sealed-9/branch".to_owned(),
                kind: DifferenceKind::ReportReceiptMismatch,
                row: None,
                expected: "reported=MaskedScan".to_owned(),
                observed: "executed=ExactAllowList".to_owned(),
            })
        );
    }

    #[test]
    fn i39_clean_execution_branch_is_accepted_before_report_plant() {
        let key = QuerySourceKey {
            query_id: 39,
            source: "sealed-9".to_owned(),
        };
        let report = BranchReportDto {
            key: key.clone(),
            branch: ExecutionBranchDto::MaskedScan,
            fallback: FallbackReasonDto::None,
            filter_cardinality: 65,
        };
        compare_i39(&I39Observed {
            reports: vec![report.clone()],
            diagnostics_reports: vec![report],
            receipts: vec![ExecutionReceiptDto {
                key,
                branch: ExecutionBranchDto::MaskedScan,
                fallback: FallbackReasonDto::None,
                row_count: 65,
                filter_cardinality: 65,
                rows_examined: 65,
                allowed_rows_examined: 65,
                vectors_scored: 65,
                graph_nodes_visited: 0,
                exact_fallback_rows_examined: 0,
                returned_candidates: 10,
                ef_effective: None,
                visited_budget: None,
                sealed: true,
            }],
            allow_list_threshold: 64,
        })
        .expect("clean I39 branch receipt");
    }

    #[test]
    fn i39_expected_case_rejects_a_receipt_from_the_wrong_lifecycle_leg() {
        let key = QuerySourceKey {
            query_id: 39,
            source: "active".to_owned(),
        };
        let report = BranchReportDto {
            key: key.clone(),
            branch: ExecutionBranchDto::ExactAllowList,
            fallback: FallbackReasonDto::None,
            filter_cardinality: 1,
        };
        let observed = I39Observed {
            reports: vec![report.clone()],
            diagnostics_reports: vec![report],
            receipts: vec![ExecutionReceiptDto {
                key: key.clone(),
                branch: ExecutionBranchDto::ExactAllowList,
                fallback: FallbackReasonDto::None,
                row_count: 1,
                filter_cardinality: 1,
                rows_examined: 1,
                allowed_rows_examined: 1,
                vectors_scored: 1,
                graph_nodes_visited: 0,
                exact_fallback_rows_examined: 0,
                returned_candidates: 1,
                ef_effective: None,
                visited_budget: None,
                sealed: false,
            }],
            allow_list_threshold: 64,
        };
        let expected = [I39ExpectedCase {
            key,
            mode: I39ExecutionModeDto::ExactScan {
                source_may_match: true,
            },
            row_count: 1,
            filter_cardinality: 1,
            allow_list_threshold: 64,
            rows_examined: 1,
            allowed_rows_examined: 1,
            vectors_scored: 1,
            graph_nodes_visited: 0,
            exact_fallback_rows_examined: 0,
            returned_candidates: 1,
            ef_effective: None,
            visited_budget: None,
            sealed: true,
        }];
        let error = match compare_i39_expected(&expected, &observed) {
            Err(error) => error,
            Ok(()) => panic!(
                "I39.executed-branch.v2 query=39 source=active expected sealed=true observed sealed=false"
            ),
        };
        assert!(
            error.contains(
                "I39.executed-branch.v2 query=39 source=active expected sealed=true observed sealed=false"
            ),
            "wrong I39 expected-case failure: {error}"
        );
    }

    #[test]
    fn i39_exact_work_counter_plant_is_rejected() {
        let key = QuerySourceKey {
            query_id: 3901,
            source: "active".to_owned(),
        };
        let report = BranchReportDto {
            key: key.clone(),
            branch: ExecutionBranchDto::ExactAllowList,
            fallback: FallbackReasonDto::None,
            filter_cardinality: 1,
        };
        let expected = [I39ExpectedCase {
            key: key.clone(),
            mode: I39ExecutionModeDto::ExactScan {
                source_may_match: true,
            },
            row_count: 1,
            filter_cardinality: 1,
            allow_list_threshold: 64,
            rows_examined: 2,
            allowed_rows_examined: 1,
            vectors_scored: 1,
            graph_nodes_visited: 0,
            exact_fallback_rows_examined: 0,
            returned_candidates: 1,
            ef_effective: None,
            visited_budget: None,
            sealed: false,
        }];
        let observed = I39Observed {
            reports: vec![report.clone()],
            diagnostics_reports: vec![report],
            receipts: vec![ExecutionReceiptDto {
                key,
                branch: ExecutionBranchDto::ExactAllowList,
                fallback: FallbackReasonDto::None,
                row_count: 1,
                filter_cardinality: 1,
                rows_examined: 1,
                allowed_rows_examined: 1,
                vectors_scored: 1,
                graph_nodes_visited: 0,
                exact_fallback_rows_examined: 0,
                returned_candidates: 1,
                ef_effective: None,
                visited_budget: None,
                sealed: false,
            }],
            allow_list_threshold: 64,
        };
        let error = match compare_i39_expected(&expected, &observed) {
            Err(error) => error,
            Ok(()) => panic!(
                "I39.executed-branch.v2 query=3901 source=active expected rows_examined=2 observed rows_examined=1"
            ),
        };
        assert!(
            error.contains(
                "I39.executed-branch.v2 query=3901 source=active expected rows_examined=2 observed rows_examined=1"
            ),
            "wrong I39 counter failure: {error}"
        );
    }

    #[test]
    fn i39_missing_production_receipt_is_a_named_failure() {
        let key = QuerySourceKey {
            query_id: 1,
            source: "active".to_owned(),
        };
        let report = BranchReportDto {
            key,
            branch: ExecutionBranchDto::ExactAllowList,
            fallback: FallbackReasonDto::None,
            filter_cardinality: 0,
        };
        let error = compare_i39(&I39Observed {
            reports: vec![report.clone()],
            diagnostics_reports: vec![report],
            receipts: Vec::new(),
            allow_list_threshold: 64,
        })
        .unwrap_err();
        assert_eq!(
            error,
            "I39.executed-branch.v2: missing production execution receipt query=1 source=active"
        );
    }

    #[test]
    fn i38_plant_reports_unsound_prune_and_missing_exact_hit() {
        let row = SourceMetadataRowDto {
            source: "sealed-a".to_owned(),
            row_id: 0,
            document_id: 77,
            cells: BTreeMap::from([(0, ScalarCell::I64(10))]),
        };
        let input = I38Input {
            sources: vec![I38SourceDto {
                source: "sealed-a".to_owned(),
                sealed: true,
                range: SourceRangeDto::Bounded { min: 10, max: 10 },
            }],
            rows: vec![row],
            live: BTreeSet::from([("sealed-a".to_owned(), 0)]),
            predicate: PredicateDto::Eq {
                column: 0,
                value: ScalarCell::I64(10),
            },
            unfiltered_exact: vec![ExactHitDto {
                source: "sealed-a".to_owned(),
                row_id: 0,
                document_id: 77,
                distance_bits: 0x3f80_0000,
            }],
            expected_delete_records: 0,
        };
        let observed = I38Observed {
            filtered_exact: Vec::new(),
            pruned_sources: BTreeSet::from(["sealed-a".to_owned()]),
            reports: vec![BranchReportDto {
                key: QuerySourceKey {
                    query_id: 38,
                    source: "sealed-a".to_owned(),
                },
                branch: ExecutionBranchDto::Pruned,
                fallback: FallbackReasonDto::None,
                filter_cardinality: 0,
            }],
            execution_receipts: vec![ExecutionReceiptDto {
                key: QuerySourceKey {
                    query_id: 38,
                    source: "sealed-a".to_owned(),
                },
                branch: ExecutionBranchDto::Pruned,
                fallback: FallbackReasonDto::None,
                row_count: 1,
                filter_cardinality: 0,
                rows_examined: 0,
                allowed_rows_examined: 0,
                vectors_scored: 0,
                graph_nodes_visited: 0,
                exact_fallback_rows_examined: 0,
                returned_candidates: 0,
                ef_effective: None,
                visited_budget: None,
                sealed: true,
            }],
            allow_list_threshold: 64,
            wal_delete_records: 0,
        };
        let error = match compare_i38(&input, &observed) {
            Err(error) => error,
            Ok(()) => panic!(
                "I38.pruning-soundness.v2 source=sealed-a unsound_prune matching_row=0; missing_exact_hit document=77 distance_bits=1065353216"
            ),
        };
        assert!(
            error.contains("I38.pruning-soundness.v2 source=sealed-a unsound_prune matching_row=0")
                && error.contains("missing_exact_hit document=77 distance_bits=1065353216"),
            "unexpected comparator failure: {error}"
        );
        assert_eq!(
            attest_i38(&input, &observed).first_difference,
            Some(FirstDifference {
                checker_id: I38_CHECKER_ID,
                path: "source=sealed-a/row=0/prune-decision".to_owned(),
                kind: DifferenceKind::UnsoundPrune,
                row: Some(0),
                expected: "retained document=77 distance_bits=1065353216".to_owned(),
                observed: "pruned".to_owned(),
            })
        );
    }

    #[test]
    fn i38_clean_result_is_accepted_before_drop_and_prune_plant() {
        let row = SourceMetadataRowDto {
            source: "sealed-a".to_owned(),
            row_id: 0,
            document_id: 77,
            cells: BTreeMap::from([(0, ScalarCell::I64(10))]),
        };
        let hit = ExactHitDto {
            source: "sealed-a".to_owned(),
            row_id: 0,
            document_id: 77,
            distance_bits: 0x3f80_0000,
        };
        let input = I38Input {
            sources: vec![I38SourceDto {
                source: "sealed-a".to_owned(),
                sealed: true,
                range: SourceRangeDto::Bounded { min: 10, max: 10 },
            }],
            rows: vec![row],
            live: BTreeSet::from([("sealed-a".to_owned(), 0)]),
            predicate: PredicateDto::Eq {
                column: 0,
                value: ScalarCell::I64(10),
            },
            unfiltered_exact: vec![hit.clone()],
            expected_delete_records: 0,
        };
        compare_i38(
            &input,
            &I38Observed {
                filtered_exact: vec![hit],
                pruned_sources: BTreeSet::new(),
                reports: vec![BranchReportDto {
                    key: QuerySourceKey {
                        query_id: 38,
                        source: "sealed-a".to_owned(),
                    },
                    branch: ExecutionBranchDto::ExactAllowList,
                    fallback: FallbackReasonDto::None,
                    filter_cardinality: 1,
                }],
                execution_receipts: vec![ExecutionReceiptDto {
                    key: QuerySourceKey {
                        query_id: 38,
                        source: "sealed-a".to_owned(),
                    },
                    branch: ExecutionBranchDto::ExactAllowList,
                    fallback: FallbackReasonDto::None,
                    row_count: 1,
                    filter_cardinality: 1,
                    rows_examined: 1,
                    allowed_rows_examined: 1,
                    vectors_scored: 1,
                    graph_nodes_visited: 0,
                    exact_fallback_rows_examined: 0,
                    returned_candidates: 1,
                    ef_effective: None,
                    visited_budget: None,
                    sealed: true,
                }],
                allow_list_threshold: 64,
                wal_delete_records: 0,
            },
        )
        .expect("clean I38 result");
    }

    #[test]
    fn i38_requires_one_report_and_receipt_for_every_required_source() {
        let input = I38Input {
            sources: vec![I38SourceDto {
                source: "sealed-required".to_owned(),
                sealed: true,
                range: SourceRangeDto::Unstamped,
            }],
            rows: vec![SourceMetadataRowDto {
                source: "sealed-required".to_owned(),
                row_id: 0,
                document_id: 77,
                cells: BTreeMap::from([(0, ScalarCell::I64(10))]),
            }],
            live: BTreeSet::from([("sealed-required".to_owned(), 0)]),
            predicate: PredicateDto::Eq {
                column: 0,
                value: ScalarCell::I64(10),
            },
            unfiltered_exact: vec![ExactHitDto {
                source: "sealed-required".to_owned(),
                row_id: 0,
                document_id: 77,
                distance_bits: 0x3f80_0000,
            }],
            expected_delete_records: 0,
        };
        let observed = I38Observed {
            filtered_exact: input.unfiltered_exact.clone(),
            pruned_sources: BTreeSet::new(),
            reports: Vec::new(),
            execution_receipts: Vec::new(),
            allow_list_threshold: 64,
            wal_delete_records: 0,
        };
        let error = match compare_i38(&input, &observed) {
            Err(error) => error,
            Ok(()) => panic!(
                "I38.pruning-soundness.v2 source=sealed-required missing public branch report"
            ),
        };
        assert_eq!(
            error,
            "I38.pruning-soundness.v2 source=sealed-required missing public branch report"
        );
    }

    #[test]
    fn i38_refuses_a_live_baseline_hit_without_source_metadata() {
        let input = I38Input {
            sources: vec![I38SourceDto {
                source: "sealed-missing".to_owned(),
                sealed: true,
                range: SourceRangeDto::Unstamped,
            }],
            rows: Vec::new(),
            live: BTreeSet::from([("sealed-missing".to_owned(), 7)]),
            predicate: PredicateDto::Exists(1),
            unfiltered_exact: vec![ExactHitDto {
                source: "sealed-missing".to_owned(),
                row_id: 7,
                document_id: 71,
                distance_bits: 0x3f80_0000,
            }],
            expected_delete_records: 0,
        };
        assert_eq!(
            expected_i38(&input).unwrap_err(),
            "I38.pruning-soundness.v2 missing metadata source=sealed-missing row=7"
        );
    }
}
