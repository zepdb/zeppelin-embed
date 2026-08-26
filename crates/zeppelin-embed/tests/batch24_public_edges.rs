#![allow(clippy::expect_used)]

use std::io::IoSlice;
use std::path::Path;

use zeppelin_embed::fts::index::{
    DEFAULT_FIELD, Document, FieldId, IndexError, LexicalIndex, SegmentIndex,
};
use zeppelin_embed::fts::phonetic;
use zeppelin_embed::fts::postings::{
    BLOCK_META_LEN, BlockMeta, DEFAULT_POSTINGS_PER_BLOCK, EncodedPostings, PostingsError,
};
use zeppelin_embed::fts::sealed::SealedSegment;
use zeppelin_embed::fts::tokenizer::vocab::Vocabulary;
use zeppelin_embed::fts::tokenizer::{
    Analyzer, TokenFlags, TokenOffset, TokenizerConfig, TokenizerError,
};
use zeppelin_embed::ingest::wal_payload::{
    PayloadError, UPSERT_V2_TYPED_COLUMNS, UPSERT_V2_VECTOR, decode_upsert_v2, encode_upsert_v2,
};
use zeppelin_embed::ingest::{DocId, DocumentVersion, IngestDocument, Revision};
use zeppelin_embed::kernels::{
    Bit4Row, Bit4Rows4, GatherShapeError, KernelVariant, prefetch_bit4_row_group,
    prefetch_bit4_rows,
};
use zeppelin_embed::meta::{
    BuildError, Column, ColumnDefinition, ColumnId, ColumnInput, ColumnStoreBuilder, ColumnType,
    ColumnValue, DocBitmap, PredicateValue, Schema,
};
use zeppelin_embed::vfs::crash::{CrashStateClass, CrashStateKind, MemoryVfs};
use zeppelin_embed::vfs::{SyncKind, Vfs};

#[test]
fn tokenizer_byte_offsets_flags_and_vocabulary_are_typed_and_reversible() {
    let mut vocabulary = Vocabulary::new();
    vocabulary
        .declare("airship", &[&["air", "ship"][..]])
        .expect("valid vocabulary");
    assert_eq!(vocabulary.len(), 2);
    assert_eq!(vocabulary.longest_surface_terms(), 2);
    assert_eq!(
        vocabulary.canonical_for(&["air".to_owned(), "ship".to_owned()]),
        Some("airship")
    );

    let analyzer = Analyzer::new(TokenizerConfig::voice().with_vocabulary(vocabulary.clone()))
        .expect("compile voice analyzer");
    assert_eq!(analyzer.config().vocabulary, vocabulary);
    let tokens = analyzer
        .analyze_bytes(b"an air ship")
        .expect("analyze valid bytes");
    assert!(tokens.iter().any(|token| token.term == "airship"));
    assert!(matches!(
        analyzer.analyze_bytes(&[b'a', 0xff, b'b']),
        Err(TokenizerError::InvalidUtf8 { offset: 1 })
    ));

    let flags = TokenFlags::NO_FUZZY.union(TokenFlags::VARIANT);
    assert!(flags.contains(TokenFlags::NO_FUZZY));
    assert!(flags.contains(TokenFlags::VARIANT));
    assert!(!TokenFlags::empty().contains(TokenFlags::VARIANT));
    assert_eq!(flags.bits(), 3);
    assert_eq!(TokenOffset { start: 1, end: 3 }.slice("abcd"), Some("bc"));
    assert_eq!(TokenOffset { start: 1, end: 2 }.slice("éclair"), None);
    assert!(vocabulary.remove("airship"));
    assert!(vocabulary.is_empty());
}

#[test]
fn phonetic_encoder_handles_doubled_and_context_sensitive_consonants() {
    for (term, expected) in [
        ("Abbey", "AP"),
        ("Cia", "X"),
        ("Cedar", "STR"),
        ("Accord", "AKRT"),
        ("Dgma", "TM"),
        ("Coffee", "KF"),
    ] {
        assert_eq!(phonetic::encode(term), expected, "encoding of {term:?}");
    }
}

#[test]
fn sealed_lexical_accessors_report_exact_field_and_document_lengths() {
    let analyzer = Analyzer::new(TokenizerConfig::text_default()).expect("compile analyzer");
    let title = FieldId(7);
    let mut first = Document::with_text("bronze zeppelin airship");
    first.set(title, "airship title");
    let second = Document::with_text("silver");
    let mut active = SegmentIndex::new();
    assert!(active.is_empty());
    assert_eq!(
        active.push_document(&analyzer, &first).expect("first row"),
        0
    );
    assert_eq!(
        active
            .push_document(&analyzer, &second)
            .expect("second row"),
        1
    );
    let sealed = SealedSegment::seal(&active).expect("seal lexical segment");

    assert_eq!(sealed.row_count(), 2);
    assert!(!sealed.is_empty());
    assert_eq!(sealed.postings_per_block(), DEFAULT_POSTINGS_PER_BLOCK);
    assert_eq!(sealed.field_length(0, DEFAULT_FIELD), 3);
    assert_eq!(sealed.field_length(0, title), 2);
    assert_eq!(sealed.field_length(1, title), 0);
    assert_eq!(sealed.field_length(99, title), 0);
    assert_eq!(sealed.document_length(0), 5);
    assert_eq!(sealed.document_length(1), 1);
    assert_eq!(sealed.document_length(99), 0);
    assert_eq!(sealed.total_lengths(), &[5, 1]);
    assert_eq!(sealed.total_tokens(), 6);
    assert_eq!(
        sealed.fields().collect::<Vec<_>>(),
        vec![DEFAULT_FIELD, title]
    );
    assert_eq!(sealed.document_frequency(b"airship", &[DEFAULT_FIELD]), 1);
    assert_eq!(
        sealed.document_frequency(b"airship", &[DEFAULT_FIELD, title]),
        1
    );

    let mut live = LexicalIndex::new();
    live.push_sealed_with_live_rows(sealed.clone(), &DocBitmap::from_ids([0]))
        .expect("one exact live row");
    assert_eq!(live.segments().len(), 1);
    assert_eq!(live.document_count(), 1);
    assert_eq!(live.total_tokens(), 5);
    assert_eq!(
        live.document_frequency(b"airship", &[DEFAULT_FIELD, title]),
        1
    );
    let mut rejected = LexicalIndex::new();
    assert_eq!(
        rejected.push_sealed_with_live_rows(sealed, &DocBitmap::from_ids([2])),
        Err(IndexError::LiveRowOutOfRange {
            segment: 0,
            row: 2,
            row_count: 2,
        })
    );
}

#[test]
fn empty_typed_columns_keep_all_physical_shapes_and_type_mismatches_atomic() {
    let definitions = [
        (ColumnId::new(1), "u64", ColumnType::U64),
        (ColumnId::new(2), "i64", ColumnType::I64),
        (ColumnId::new(3), "f64", ColumnType::F64),
        (ColumnId::new(4), "bool", ColumnType::Bool),
        (ColumnId::new(5), "dictionary", ColumnType::DictionaryString),
        (ColumnId::new(6), "raw", ColumnType::RawString),
    ];
    let schema = Schema::new(
        definitions
            .iter()
            .map(|(id, name, kind)| ColumnDefinition::new(*id, *name, *kind, true))
            .collect(),
    )
    .expect("complete nullable schema");
    let store = ColumnStoreBuilder::new(schema.clone())
        .finish()
        .expect("finish empty columns");
    assert_eq!(store.row_count(), 0);
    for (id, _, kind) in definitions {
        let column = store.column(id).expect("declared empty column");
        assert_eq!(column.column_type(), kind);
        assert_eq!(column.len(), 0);
        assert!(column.is_empty());
        match column {
            Column::U64(column) => assert!(column.is_empty()),
            Column::I64(column) => assert!(column.is_empty()),
            Column::F64(column) => assert!(column.is_empty()),
            Column::Bool(column) => assert!(column.is_empty()),
            Column::DictionaryString(column) => assert!(column.is_empty()),
            Column::RawString(column) => assert!(column.is_empty()),
        }
    }

    for (value, actual) in [
        (ColumnValue::U64(1), ColumnType::U64),
        (ColumnValue::I64(-1), ColumnType::I64),
        (ColumnValue::F64(1.5), ColumnType::F64),
    ] {
        let mut builder = ColumnStoreBuilder::new(schema.clone());
        let error = builder
            .push_row(
                24,
                &[ColumnInput {
                    column: ColumnId::new(4),
                    value,
                }],
            )
            .expect_err("numeric value must not enter a Boolean column");
        assert_eq!(
            error,
            BuildError::TypeMismatch {
                column: ColumnId::new(4),
                expected: ColumnType::Bool,
                actual,
            }
        );
        assert_eq!(
            builder
                .finish()
                .expect("failed row did not poison builder")
                .row_count(),
            0
        );
    }
}

#[test]
fn memory_vfs_handle_and_path_operations_preserve_bytes_and_snapshot_isolation() {
    let vfs = MemoryVfs::new();
    let directory = Path::new("batch24");
    let first = directory.join("first");
    let second = directory.join("second");
    vfs.write(&first, b"ab").expect("seed file");
    let mut handle = vfs.open_append(&first).expect("open append handle");
    handle.append(b"cd").expect("append bytes");
    let mut slices = [IoSlice::new(b"ef"), IoSlice::new(b"gh")];
    handle
        .append_vectored(&mut slices)
        .expect("append vectored bytes");
    handle.sync(SyncKind::Barrier).expect("sync handle");
    assert_eq!(vfs.open(&first).expect("file length"), 8);
    assert_eq!(vfs.read_range(&first, 2, 4).expect("middle range"), b"cdef");
    assert!(vfs.read_range(&first, 99, 4).expect("past end").is_empty());
    vfs.rename(&first, &second).expect("rename file");
    assert_eq!(
        vfs.list(directory).expect("list directory"),
        vec![second.clone()]
    );
    let snapshot = vfs.snapshot().expect("snapshot filesystem");
    vfs.delete(&second).expect("delete live file");
    assert!(vfs.files().expect("live files").is_empty());
    assert_eq!(snapshot.read(&second).expect("snapshot bytes"), b"abcdefgh");
    assert_eq!(
        CrashStateKind::Prefix {
            completed_operations: 1,
        }
        .class(),
        CrashStateClass::Prefix
    );
    assert_eq!(
        CrashStateKind::RenameWithOldContent { operation_index: 2 }.class(),
        CrashStateClass::RenameWithOldContent
    );
}

#[test]
fn packed_row_handles_and_posting_wrappers_keep_validated_lengths_visible() {
    let region = [0x12_u8, 0x34, 0x56, 0x78, 0x9a, 0xbc];
    let row = Bit4Row::from_mapped_region(&region, 2, 2).expect("bounded packed row");
    assert_eq!(format!("{row:?}"), "Bit4Row { len: 2 }");
    assert!(Bit4Row::from_mapped_region(&region, usize::MAX, 2).is_none());
    assert!(Bit4Row::from_mapped_region(&region, 5, 2).is_none());
    let rows = Bit4Rows4::from_rows([row; 4], 2).expect("equal four-row shape");
    assert_eq!(format!("{rows:?}"), "Bit4Rows4 { row_bytes: 2 }");
    prefetch_bit4_rows(&[row]);
    prefetch_bit4_row_group(&rows);
    assert!(matches!(
        Bit4Rows4::from_rows([row; 4], 3),
        Err(GatherShapeError::RowLength {
            index: 0,
            expected: 3,
            actual: 2,
        })
    ));
    let scalar = KernelVariant::scalar();
    assert!(format!("{scalar:?}").contains("KernelVariant"));
    let selected = KernelVariant::selected();
    assert!(format!("{selected:?}").contains("arm"));

    assert_eq!(
        BlockMeta::read(&[]),
        Err(PostingsError::Truncated {
            needed: BLOCK_META_LEN,
            available: 0,
        })
    );
    let wrapped = EncodedPostings::from_bytes(vec![1, 2, 3]);
    assert_eq!(wrapped.as_bytes(), &[1, 2, 3]);
}

#[test]
fn bitmap_wal_payload_rejects_invalid_vectors_and_preserves_every_numeric_column_kind() {
    let version = DocumentVersion::new(DocId::new(24), Revision::new(7));
    assert_eq!(
        encode_upsert_v2(&IngestDocument::new(version, Vec::new())),
        Err(PayloadError::EmptyVector)
    );
    assert_eq!(
        encode_upsert_v2(&IngestDocument::new(version, vec![1.0, f32::NAN])),
        Err(PayloadError::NonFiniteVector { index: 1 })
    );

    let mut decoded_empty = Vec::new();
    decoded_empty.extend_from_slice(&UPSERT_V2_VECTOR.to_le_bytes());
    decoded_empty.extend_from_slice(&version.doc_id().get().to_le_bytes());
    decoded_empty.extend_from_slice(&version.revision().get().to_le_bytes());
    decoded_empty.extend_from_slice(&0_u32.to_le_bytes());
    assert_eq!(
        decode_upsert_v2(&decoded_empty),
        Err(PayloadError::EmptyVector)
    );
    let mut decoded_non_finite = Vec::new();
    decoded_non_finite.extend_from_slice(&UPSERT_V2_VECTOR.to_le_bytes());
    decoded_non_finite.extend_from_slice(&version.doc_id().get().to_le_bytes());
    decoded_non_finite.extend_from_slice(&version.revision().get().to_le_bytes());
    decoded_non_finite.extend_from_slice(&1_u32.to_le_bytes());
    decoded_non_finite.extend_from_slice(&f32::NAN.to_bits().to_le_bytes());
    assert_eq!(
        decode_upsert_v2(&decoded_non_finite),
        Err(PayloadError::NonFiniteVector { index: 0 })
    );

    let document = IngestDocument::new(version, vec![1.0]).with_columns(vec![
        (ColumnId::new(1), PredicateValue::I64(-24)),
        (ColumnId::new(2), PredicateValue::F64(2.5)),
        (ColumnId::new(3), PredicateValue::Bool(true)),
    ]);
    let encoded = encode_upsert_v2(&document).expect("encode typed bitmap payload");
    let decoded = decode_upsert_v2(&encoded).expect("decode typed bitmap payload");
    assert_eq!(decoded.version(), version);
    assert_eq!(decoded.vector(), &[1.0]);
    assert_eq!(decoded.columns(), document.columns());

    let mut reserved = encoded.clone();
    let first_reserved = 4 + 24 + 4 + 4 + 4 + 4 + 1;
    *reserved
        .get_mut(first_reserved)
        .expect("encoded column has reserved bytes") = 1;
    assert_eq!(decode_upsert_v2(&reserved), Err(PayloadError::Reserved(1)));

    let mut null_column = Vec::new();
    null_column.extend_from_slice(&(UPSERT_V2_VECTOR | UPSERT_V2_TYPED_COLUMNS).to_le_bytes());
    null_column.extend_from_slice(&version.doc_id().get().to_le_bytes());
    null_column.extend_from_slice(&version.revision().get().to_le_bytes());
    null_column.extend_from_slice(&1_u32.to_le_bytes());
    null_column.extend_from_slice(&1.0_f32.to_bits().to_le_bytes());
    null_column.extend_from_slice(&1_u32.to_le_bytes());
    null_column.extend_from_slice(&ColumnId::new(1).get().to_le_bytes());
    null_column.push(0);
    null_column.extend_from_slice(&[0_u8; 3]);
    null_column.extend_from_slice(&0_u32.to_le_bytes());
    assert_eq!(
        decode_upsert_v2(&null_column),
        Err(PayloadError::ValueKind(0))
    );

    let mut no_vector = Vec::new();
    no_vector.extend_from_slice(&0_u32.to_le_bytes());
    no_vector.extend_from_slice(&version.doc_id().get().to_le_bytes());
    no_vector.extend_from_slice(&version.revision().get().to_le_bytes());
    let decoded = decode_upsert_v2(&no_vector).expect("decode explicit absent vector");
    assert!(decoded.vector().is_empty());
    assert!(decoded.text().is_none());
    assert!(decoded.columns().is_empty());
}
