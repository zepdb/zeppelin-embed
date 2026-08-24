#![allow(clippy::expect_used)]

use zeppelin_embed::format::golden::decode_hex;
use zeppelin_embed::ingest::wal_payload::{
    DELETE_V1, METADATA_EDIT_V1, MetadataEdit, MetadataValue, MutationPayload, PayloadError,
    UPSERT_V1, UPSERT_WITH_METADATA_V1, UPSERT_WITH_TIMESTAMP_AND_METADATA_V1,
    UPSERT_WITH_TIMESTAMP_V1, decode_delete, decode_metadata_edit, decode_mutation, decode_upsert,
    decode_upsert_with_metadata, decode_upsert_with_timestamp,
    decode_upsert_with_timestamp_and_metadata, encode_delete, encode_metadata_edit, encode_upsert,
    encode_upsert_with_metadata, encode_upsert_with_timestamp,
    encode_upsert_with_timestamp_and_metadata,
};
use zeppelin_embed::ingest::{DocId, DocumentVersion, IngestDocument, Revision};
use zeppelin_embed::meta::ColumnId;
use zeppelin_embed::wal::LogSeq;
use zeppelin_embed::wal::record::{WalRecord, decode_record, encode_record};

fn fixture(text: &str) -> Vec<u8> {
    decode_hex(text).expect("fixture hex")
}

fn assert_record_golden(op: u16, payload: &[u8], expected: &str, decoded: MutationPayload) {
    let encoded = encode_record(WalRecord {
        seq: LogSeq::new(0x0102_0304_0506_0708),
        op,
        payload,
    })
    .expect("encode frozen WAL frame");
    assert_eq!(encoded, fixture(expected));
    let record = decode_record(&encoded)
        .expect("decode frozen WAL frame")
        .record;
    assert_eq!(decode_mutation(record.op, record.payload), Ok(decoded));
}

#[test]
fn wal_upsert_payload_v1_is_byte_exact() {
    let document = IngestDocument::new(
        DocumentVersion::new(
            DocId::new(0x0011_2233_4455_6677_8899_aabb_ccdd_eeff),
            Revision::new(0x1122_3344_5566_7788),
        ),
        vec![1.5, -2.25],
    );
    let payload = encode_upsert(&document).expect("encode upsert");
    assert_record_golden(
        UPSERT_V1,
        &payload,
        include_str!("fixtures/format/wal_upsert_record_v1.hex"),
        MutationPayload::Upsert(document),
    );
}

#[test]
fn wal_timestamped_upsert_payload_v1_is_byte_exact() {
    let document = IngestDocument::new(
        DocumentVersion::new(
            DocId::new(0x0011_2233_4455_6677_8899_aabb_ccdd_eeff),
            Revision::new(0x1122_3344_5566_7788),
        ),
        vec![1.5, -2.25],
    )
    .with_timestamp(-1234);
    let payload = encode_upsert_with_timestamp(&document).expect("encode timestamped upsert");
    assert_eq!(decode_upsert_with_timestamp(&payload), Ok(document.clone()));
    assert_record_golden(
        UPSERT_WITH_TIMESTAMP_V1,
        &payload,
        include_str!("fixtures/format/wal_upsert_timestamp_record_v1.hex"),
        MutationPayload::Upsert(document),
    );
}

#[test]
fn wal_stored_metadata_upsert_payloads_v1_are_byte_exact() {
    let plain = IngestDocument::new(
        DocumentVersion::new(
            DocId::new(0x0011_2233_4455_6677_8899_aabb_ccdd_eeff),
            Revision::new(0x1122_3344_5566_7788),
        ),
        vec![1.5, -2.25],
    )
    .with_metadata(b"meta".to_vec());
    let payload = encode_upsert_with_metadata(&plain).expect("encode metadata upsert");
    assert_eq!(decode_upsert_with_metadata(&payload), Ok(plain.clone()));
    assert_record_golden(
        UPSERT_WITH_METADATA_V1,
        &payload,
        include_str!("fixtures/format/wal_upsert_metadata_record_v1.hex"),
        MutationPayload::Upsert(plain),
    );

    let timestamped = IngestDocument::new(
        DocumentVersion::new(
            DocId::new(0x0011_2233_4455_6677_8899_aabb_ccdd_eeff),
            Revision::new(0x1122_3344_5566_7788),
        ),
        vec![1.5, -2.25],
    )
    .with_timestamp(-1234)
    .with_metadata(b"meta".to_vec());
    let payload = encode_upsert_with_timestamp_and_metadata(&timestamped)
        .expect("encode timestamped metadata upsert");
    assert_eq!(
        decode_upsert_with_timestamp_and_metadata(&payload),
        Ok(timestamped.clone())
    );
    assert_record_golden(
        UPSERT_WITH_TIMESTAMP_AND_METADATA_V1,
        &payload,
        include_str!("fixtures/format/wal_upsert_timestamp_metadata_record_v1.hex"),
        MutationPayload::Upsert(timestamped),
    );
}

#[test]
fn wal_delete_payload_v1_is_byte_exact() {
    let doc_ids = vec![
        DocId::new(0x0011_2233_4455_6677_8899_aabb_ccdd_eeff),
        DocId::new(0xffee_ddcc_bbaa_9988_7766_5544_3322_1100),
    ];
    let payload = encode_delete(&doc_ids).expect("encode delete");
    assert_record_golden(
        DELETE_V1,
        &payload,
        include_str!("fixtures/format/wal_delete_record_v1.hex"),
        MutationPayload::Delete(doc_ids),
    );
}

#[test]
fn wal_metadata_edit_payload_v1_is_byte_exact() {
    let edit = MetadataEdit::new(
        DocumentVersion::new(
            DocId::new(0x0011_2233_4455_6677_8899_aabb_ccdd_eeff),
            Revision::new(0x1122_3344_5566_7788),
        ),
        ColumnId::new(0xa1b2_c3d4),
        MetadataValue::String("blue".to_owned()),
    );
    let payload = encode_metadata_edit(&edit).expect("encode metadata edit");
    assert_record_golden(
        METADATA_EDIT_V1,
        &payload,
        include_str!("fixtures/format/wal_metadata_edit_record_v1.hex"),
        MutationPayload::MetadataEdit(edit),
    );
}

#[test]
fn mutation_payload_decoder_rejects_corruption_typed() {
    for (op, payload) in [
        (UPSERT_V1, &b"short"[..]),
        (DELETE_V1, &b"short"[..]),
        (METADATA_EDIT_V1, &b"short"[..]),
        (u16::MAX, &b""[..]),
    ] {
        assert!(decode_mutation(op, payload).is_err());
    }
}

#[test]
fn mutation_operation_ids_are_append_only() {
    assert_eq!(
        (
            UPSERT_V1,
            DELETE_V1,
            METADATA_EDIT_V1,
            UPSERT_WITH_TIMESTAMP_V1,
            UPSERT_WITH_METADATA_V1,
            UPSERT_WITH_TIMESTAMP_AND_METADATA_V1,
        ),
        (1, 2, 3, 4, 5, 6)
    );
}

fn edit(value: MetadataValue) -> MetadataEdit {
    MetadataEdit::new(
        DocumentVersion::new(DocId::new(7), Revision::new(9)),
        ColumnId::new(11),
        value,
    )
}

#[test]
fn every_metadata_value_has_a_canonical_round_trip() {
    for value in [
        MetadataValue::Null,
        MetadataValue::U64(u64::MAX),
        MetadataValue::I64(i64::MIN),
        MetadataValue::F64(-12.5),
        MetadataValue::Bool(false),
        MetadataValue::Bool(true),
        MetadataValue::String("ocean".to_owned()),
    ] {
        let expected = edit(value);
        let encoded = encode_metadata_edit(&expected).expect("encode metadata value");
        assert_eq!(decode_metadata_edit(&encoded), Ok(expected));
    }
}

#[test]
fn mutation_payload_decoder_rejects_each_noncanonical_field_typed() {
    let document = IngestDocument::new(
        DocumentVersion::new(DocId::new(1), Revision::new(2)),
        vec![3.0],
    );
    let valid_upsert = encode_upsert(&document).expect("valid upsert");

    let mut wrong_version = valid_upsert.clone();
    wrong_version[0..2].copy_from_slice(&2_u16.to_le_bytes());
    assert_eq!(decode_upsert(&wrong_version), Err(PayloadError::Version(2)));

    let mut nonzero_flags = valid_upsert.clone();
    nonzero_flags[2..4].copy_from_slice(&1_u16.to_le_bytes());
    assert_eq!(decode_upsert(&nonzero_flags), Err(PayloadError::Flags(1)));

    let mut empty_vector = valid_upsert.clone();
    empty_vector[28..32].copy_from_slice(&0_u32.to_le_bytes());
    empty_vector.truncate(32);
    assert_eq!(decode_upsert(&empty_vector), Err(PayloadError::EmptyVector));

    let mut nonfinite_vector = valid_upsert.clone();
    nonfinite_vector[32..36].copy_from_slice(&f32::NAN.to_bits().to_le_bytes());
    assert_eq!(
        decode_upsert(&nonfinite_vector),
        Err(PayloadError::NonFiniteVector { index: 0 })
    );
    assert_eq!(
        encode_upsert(&IngestDocument::new(
            document.version(),
            vec![f32::INFINITY]
        )),
        Err(PayloadError::NonFiniteVector { index: 0 })
    );
    assert_eq!(
        encode_upsert(&IngestDocument::new(document.version(), Vec::new())),
        Err(PayloadError::EmptyVector)
    );

    let empty_delete = [1_u8, 0, 0, 0, 0, 0, 0, 0];
    assert_eq!(decode_delete(&empty_delete), Err(PayloadError::EmptyDelete));
    assert_eq!(encode_delete(&[]), Err(PayloadError::EmptyDelete));

    let mut reserved = encode_metadata_edit(&edit(MetadataValue::Null)).expect("metadata");
    reserved[33] = 1;
    assert_eq!(
        decode_metadata_edit(&reserved),
        Err(PayloadError::Reserved(1))
    );

    let mut unknown_kind = encode_metadata_edit(&edit(MetadataValue::Null)).expect("metadata");
    unknown_kind[32] = u8::MAX;
    assert_eq!(
        decode_metadata_edit(&unknown_kind),
        Err(PayloadError::ValueKind(u8::MAX))
    );

    let mut wrong_width = encode_metadata_edit(&edit(MetadataValue::U64(1))).expect("metadata");
    wrong_width[36..40].copy_from_slice(&7_u32.to_le_bytes());
    wrong_width.truncate(47);
    assert_eq!(
        decode_metadata_edit(&wrong_width),
        Err(PayloadError::ValueLength {
            kind: 1,
            expected: 8,
            actual: 7,
        })
    );

    let mut invalid_bool =
        encode_metadata_edit(&edit(MetadataValue::Bool(true))).expect("metadata");
    invalid_bool[40] = 2;
    assert_eq!(
        decode_metadata_edit(&invalid_bool),
        Err(PayloadError::Boolean(2))
    );

    let mut invalid_utf8 =
        encode_metadata_edit(&edit(MetadataValue::String("x".to_owned()))).expect("metadata");
    invalid_utf8[40] = u8::MAX;
    assert_eq!(decode_metadata_edit(&invalid_utf8), Err(PayloadError::Utf8));

    let mut trailing = valid_upsert;
    trailing.push(0);
    assert_eq!(
        decode_upsert(&trailing),
        Err(PayloadError::TrailingBytes(1))
    );
    assert_eq!(decode_delete(&[1, 0, 0]), Err(PayloadError::Truncated));
}

#[test]
fn payload_errors_name_the_rejected_invariant() {
    let errors = [
        PayloadError::UnknownOperation(99),
        PayloadError::LengthOverflow,
        PayloadError::Truncated,
        PayloadError::Version(2),
        PayloadError::Flags(1),
        PayloadError::Reserved(1),
        PayloadError::EmptyVector,
        PayloadError::EmptyDelete,
        PayloadError::NonFiniteVector { index: 3 },
        PayloadError::ValueKind(8),
        PayloadError::ValueLength {
            kind: 1,
            expected: 8,
            actual: 7,
        },
        PayloadError::Boolean(2),
        PayloadError::Utf8,
        PayloadError::TrailingBytes(1),
    ];
    for error in errors {
        assert!(!error.to_string().is_empty());
    }
}
