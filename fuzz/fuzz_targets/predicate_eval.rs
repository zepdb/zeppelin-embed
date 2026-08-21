#![no_main]

use libfuzzer_sys::fuzz_target;
use zeppelin_embed::meta::{
    AliveSet, ColumnDefinition, ColumnId, ColumnInput, ColumnStoreBuilder, ColumnType, ColumnValue,
    Predicate, PredicateValue, RangeBound, RangePredicate, Schema, TIMESTAMP_COLUMN, evaluate,
};

struct Cursor<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn byte(&mut self) -> u8 {
        let value = self.bytes.get(self.position).copied().unwrap_or_default();
        self.position = self.position.saturating_add(1).min(self.bytes.len());
        value
    }

    fn u64(&mut self) -> u64 {
        let mut bytes = [0_u8; 8];
        for output in &mut bytes {
            *output = self.byte();
        }
        u64::from_le_bytes(bytes)
    }

    fn string(&mut self) -> String {
        let requested = usize::from(self.byte() % 24);
        let end = self.position.saturating_add(requested).min(self.bytes.len());
        let value = self
            .bytes
            .get(self.position..end)
            .map(String::from_utf8_lossy)
            .map(|value| value.into_owned())
            .unwrap_or_default();
        self.position = end;
        value
    }
}

enum OwnedValue {
    U64(u64),
    I64(i64),
    F64(f64),
    Bool(bool),
    String(String),
}

impl OwnedValue {
    fn borrowed(&self) -> ColumnValue<'_> {
        match self {
            Self::U64(value) => ColumnValue::U64(*value),
            Self::I64(value) => ColumnValue::I64(*value),
            Self::F64(value) => ColumnValue::F64(*value),
            Self::Bool(value) => ColumnValue::Bool(*value),
            Self::String(value) => ColumnValue::String(value),
        }
    }
}

fn parse_column_type(cursor: &mut Cursor<'_>) -> ColumnType {
    match cursor.byte() % 6 {
        0 => ColumnType::U64,
        1 => ColumnType::I64,
        2 => ColumnType::F64,
        3 => ColumnType::Bool,
        4 => ColumnType::DictionaryString,
        _ => ColumnType::RawString,
    }
}

fn parse_owned_value(cursor: &mut Cursor<'_>, column_type: ColumnType) -> OwnedValue {
    match column_type {
        ColumnType::U64 => OwnedValue::U64(cursor.u64()),
        ColumnType::I64 => OwnedValue::I64(i64::from_le_bytes(cursor.u64().to_le_bytes())),
        ColumnType::F64 => OwnedValue::F64(f64::from_bits(cursor.u64())),
        ColumnType::Bool => OwnedValue::Bool(cursor.byte() & 1 != 0),
        ColumnType::DictionaryString | ColumnType::RawString => {
            OwnedValue::String(cursor.string())
        }
    }
}

fn parse_predicate_value(cursor: &mut Cursor<'_>, column_type: ColumnType) -> PredicateValue {
    match column_type {
        ColumnType::U64 => PredicateValue::U64(cursor.u64()),
        ColumnType::I64 => PredicateValue::I64(i64::from_le_bytes(cursor.u64().to_le_bytes())),
        ColumnType::F64 => PredicateValue::F64(f64::from_bits(cursor.u64())),
        ColumnType::Bool => PredicateValue::Bool(cursor.byte() & 1 != 0),
        ColumnType::DictionaryString | ColumnType::RawString => {
            PredicateValue::String(cursor.string())
        }
    }
}

fn select_column(cursor: &mut Cursor<'_>, types: &[ColumnType]) -> (ColumnId, ColumnType) {
    let count = types.len().saturating_add(1);
    let position = usize::from(cursor.byte()) % count;
    if position == 0 {
        return (TIMESTAMP_COLUMN, ColumnType::I64);
    }
    let column_type = types
        .get(position.saturating_sub(1))
        .copied()
        .unwrap_or(ColumnType::I64);
    let id = u32::try_from(position).unwrap_or_default();
    (ColumnId::new(id), column_type)
}

fn parse_bound(
    cursor: &mut Cursor<'_>,
    column_type: ColumnType,
) -> Option<RangeBound> {
    if cursor.byte() & 1 == 0 {
        return None;
    }
    Some(RangeBound {
        value: parse_predicate_value(cursor, column_type),
        inclusive: cursor.byte() & 1 != 0,
    })
}

fn parse_predicate(cursor: &mut Cursor<'_>, types: &[ColumnType], depth: u8) -> Predicate {
    let operation = cursor.byte() % if depth == 0 { 5 } else { 8 };
    let (column, column_type) = select_column(cursor, types);
    match operation {
        0 => Predicate::Eq {
            column,
            value: parse_predicate_value(cursor, column_type),
        },
        1 => {
            let count = usize::from(cursor.byte() % 5);
            let values = (0..count)
                .map(|_| parse_predicate_value(cursor, column_type))
                .collect();
            Predicate::In { column, values }
        }
        2 if matches!(column_type, ColumnType::U64 | ColumnType::I64 | ColumnType::F64) => {
            Predicate::Range(RangePredicate {
                column,
                lower: parse_bound(cursor, column_type),
                upper: parse_bound(cursor, column_type),
            })
        }
        2 => Predicate::Exists(column),
        3 => Predicate::Exists(column),
        4 => Predicate::IsNull(column),
        5 => {
            let count = usize::from(cursor.byte() % 4);
            Predicate::And(
                (0..count)
                    .map(|_| parse_predicate(cursor, types, depth.saturating_sub(1)))
                    .collect(),
            )
        }
        6 => {
            let count = usize::from(cursor.byte() % 4);
            Predicate::Or(
                (0..count)
                    .map(|_| parse_predicate(cursor, types, depth.saturating_sub(1)))
                    .collect(),
            )
        }
        _ => Predicate::Not(Box::new(parse_predicate(
            cursor,
            types,
            depth.saturating_sub(1),
        ))),
    }
}

fuzz_target!(|data: &[u8]| {
    let mut cursor = Cursor::new(data);
    let column_count = usize::from(cursor.byte() % 7);
    let types = (0..column_count)
        .map(|_| parse_column_type(&mut cursor))
        .collect::<Vec<_>>();
    let definitions = types
        .iter()
        .copied()
        .enumerate()
        .filter_map(|(position, column_type)| {
            let raw_id = u32::try_from(position.saturating_add(1)).ok()?;
            Some(ColumnDefinition::new(
                ColumnId::new(raw_id),
                format!("column_{raw_id}"),
                column_type,
                true,
            ))
        })
        .collect::<Vec<_>>();
    let Ok(schema) = Schema::new(definitions) else {
        return;
    };
    let mut builder = ColumnStoreBuilder::new(schema);
    let row_count = usize::from(cursor.byte() % 65);
    for _ in 0..row_count {
        let timestamp = i64::from_le_bytes(cursor.u64().to_le_bytes());
        let owned = types
            .iter()
            .copied()
            .enumerate()
            .filter_map(|(position, column_type)| {
                if cursor.byte() & 1 == 0 {
                    return None;
                }
                let raw_id = u32::try_from(position.saturating_add(1)).ok()?;
                Some((
                    ColumnId::new(raw_id),
                    parse_owned_value(&mut cursor, column_type),
                ))
            })
            .collect::<Vec<_>>();
        let inputs = owned
            .iter()
            .map(|(column, value)| ColumnInput {
                column: *column,
                value: value.borrowed(),
            })
            .collect::<Vec<_>>();
        if builder.push_row(timestamp, &inputs).is_err() {
            return;
        }
    }
    let Ok(columns) = builder.finish() else {
        return;
    };
    let mut alive = AliveSet::new(columns.row_count());
    for document in 0..columns.row_count() {
        if cursor.byte() & 1 != 0 && alive.tombstone(document).is_err() {
            return;
        }
    }
    let predicate = parse_predicate(&mut cursor, &types, 4);
    if let Ok(result) = evaluate(&predicate, &columns, &alive) {
        assert_eq!(
            result.is_subset(alive.alive_bitmap()),
            true,
            "predicate result must remain within the alive set"
        );
    }
});
