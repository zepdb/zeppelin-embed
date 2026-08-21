#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

use xxhash_rust::xxh3::xxh3_64;
use zeppelin_embed::format::FormatFamily;
use zeppelin_embed::format::frame::{
    FILE_MAGIC, FileHeader, FormatCheck, decode_header as decode_frame_header,
    encode_header as encode_frame_header,
};
use zeppelin_embed::format::golden::decode_hex;
use zeppelin_embed::wal::LogSeq;
use zeppelin_embed::wal::header::{
    WAL_HEADER_LEN, WalHeader, WalHeaderError, decode_header as decode_wal_header, encode_header,
};
use zeppelin_embed::wal::record::{
    MIN_RECORD_LEN, RecordEncodeError, RecordError, WalRecord, decode_record, encode_record,
};
use zeppelin_embed::wal::replay::{CorruptionLocation, CorruptionReason, ReplayTerminator, replay};

fn encoded(seq: u64, op: u16, payload: &[u8]) -> Vec<u8> {
    encode_record(WalRecord {
        seq: LogSeq::new(seq),
        op,
        payload,
    })
    .expect("record")
}

fn wal_bytes(first_seq: u64, records: &[(u64, u16, &[u8])]) -> Vec<u8> {
    let mut bytes = encode_header(LogSeq::new(first_seq)).expect("registered WAL header");
    for (seq, op, payload) in records {
        bytes.extend_from_slice(&encoded(*seq, *op, payload));
    }
    bytes
}

fn fixture(text: &str) -> Vec<u8> {
    decode_hex(text).expect("fixture")
}

fn extended_header(first_seq: u64, file_length: u64) -> Vec<u8> {
    let mut bytes = encode_frame_header(FileHeader {
        magic: FILE_MAGIC,
        family: FormatFamily::Wal.id(),
        version: 1,
        flags: 0,
        header_length: WAL_HEADER_LEN as u64,
        file_length,
    });
    bytes.extend_from_slice(&first_seq.to_le_bytes());
    bytes
}

#[test]
fn wal_header_is_40_bytes_with_zero_file_length_and_owned_first_sequence() {
    let encoded = encode_header(LogSeq::new(5_000)).expect("registered WAL header");
    assert_eq!(encoded.len(), WAL_HEADER_LEN);
    assert_eq!(&encoded[16..24], &40_u64.to_le_bytes());
    assert_eq!(&encoded[24..32], &0_u64.to_le_bytes());
    assert_eq!(&encoded[32..40], &5_000_u64.to_le_bytes());
}

#[test]
fn wal_header_rejects_nonzero_shared_file_length() {
    let bytes = extended_header(5_000, 9);
    assert_eq!(
        decode_wal_header(&bytes),
        Err(WalHeaderError::NonZeroFileLength { actual: 9 })
    );
}

#[test]
fn wal_header_reads_first_sequence_from_family_extension() {
    let bytes = extended_header(5_000, 0);
    assert_eq!(
        decode_wal_header(&bytes),
        Ok(WalHeader {
            first_seq: LogSeq::new(5_000),
        })
    );
}

#[test]
fn wal_replay_starts_records_at_declared_header_boundary() {
    let record = WalRecord {
        seq: LogSeq::new(5_000),
        op: 21,
        payload: b"after-extension",
    };
    let mut bytes = extended_header(5_000, 0);
    bytes.extend_from_slice(&encode_record(record).expect("record"));

    let replayed = replay(&bytes);
    assert_eq!(replayed.records, vec![record]);
    assert_eq!(replayed.terminator, ReplayTerminator::CleanEnd);
}

#[test]
fn wal_file_header_uses_shared_layout_and_declares_first_sequence() {
    let encoded = encode_header(LogSeq::new(5_000)).expect("registered WAL header");
    let expected = [
        0x5a, 0x45, 0x50, 0x45, 0x4d, 0x42, 0x45, 0x44, // magic
        0x0b, 0x00, // family
        0x01, 0x00, // version
        0x00, 0x00, 0x00, 0x00, // flags
        0x28, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // header length
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // file length
        0x88, 0x13, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // first sequence
    ];
    assert_eq!(encoded, expected);

    let decoded =
        decode_frame_header("wal", FormatFamily::Wal, &encoded).expect("shared header decodes");
    assert_eq!(decoded.header_length, WAL_HEADER_LEN as u64);
    assert_eq!(decoded.file_length, 0);
}

#[test]
fn wal_replay_rejects_each_invalid_file_header_before_records() {
    let header = encode_header(LogSeq::new(1)).expect("registered WAL header");
    let cases = [
        ("missing", Vec::new(), WalHeaderError::Missing),
        (
            "truncated",
            header[..WAL_HEADER_LEN - 1].to_vec(),
            WalHeaderError::Truncated {
                needed: WAL_HEADER_LEN,
                available: WAL_HEADER_LEN - 1,
            },
        ),
        (
            "magic",
            {
                let mut bytes = header.clone();
                bytes[4] ^= 0xff;
                bytes
            },
            WalHeaderError::WrongMagic {
                expected: *b"ZEPEMBED",
                actual: *b"ZEPE\xb2BED",
            },
        ),
        (
            "family",
            {
                let mut bytes = header.clone();
                bytes[8..10].copy_from_slice(&FormatFamily::Manifest.id().to_le_bytes());
                bytes
            },
            WalHeaderError::WrongFamily {
                expected: FormatFamily::Wal.id(),
                actual: FormatFamily::Manifest.id(),
            },
        ),
        (
            "header length",
            {
                let mut bytes = header.clone();
                bytes[16..24].copy_from_slice(&41_u64.to_le_bytes());
                bytes
            },
            WalHeaderError::InvalidHeaderLength {
                expected: WAL_HEADER_LEN as u64,
                actual: 41,
            },
        ),
        (
            "short declared header length",
            {
                let mut bytes = header.clone();
                bytes[16..24].copy_from_slice(&39_u64.to_le_bytes());
                bytes
            },
            WalHeaderError::InvalidHeaderLength {
                expected: WAL_HEADER_LEN as u64,
                actual: 39,
            },
        ),
        (
            "below shared header length",
            {
                let mut bytes = header.clone();
                bytes[16..24].copy_from_slice(&31_u64.to_le_bytes());
                bytes
            },
            WalHeaderError::InvalidHeaderLength {
                expected: WAL_HEADER_LEN as u64,
                actual: 31,
            },
        ),
        (
            "version",
            {
                let mut bytes = header;
                bytes[10..12].copy_from_slice(&2_u16.to_le_bytes());
                bytes
            },
            WalHeaderError::UnsupportedVersion {
                family: FormatFamily::Wal.id(),
                version: 2,
                minimum: 1,
                maximum: 1,
            },
        ),
    ];

    for (label, bytes, expected) in cases {
        let result = replay(&bytes);
        assert_eq!(result.records, Vec::<WalRecord<'_>>::new(), "{label}");
        assert_eq!(
            result.terminator,
            ReplayTerminator::InvalidHeader(expected),
            "{label}"
        );
    }
}

#[test]
fn wal_replay_uses_header_first_sequence_and_reports_first_mismatch() {
    let starting_at_one = wal_bytes(1, &[(1, 10, b"one")]);
    let result = replay(&starting_at_one);
    assert_eq!(result.records.len(), 1);
    assert_eq!(result.terminator, ReplayTerminator::CleanEnd);

    let rotated = wal_bytes(
        5_000,
        &[(5_000, 20, b"five-thousand"), (5_001, 21, b"next")],
    );
    let result = replay(&rotated);
    assert_eq!(
        result.records,
        vec![
            WalRecord {
                seq: LogSeq::new(5_000),
                op: 20,
                payload: b"five-thousand",
            },
            WalRecord {
                seq: LogSeq::new(5_001),
                op: 21,
                payload: b"next",
            },
        ]
    );
    assert_eq!(result.terminator, ReplayTerminator::CleanEnd);

    let mismatch = wal_bytes(5_000, &[(5_001, 30, b"wrong-first")]);
    let result = replay(&mismatch);
    assert_eq!(result.records, Vec::<WalRecord<'_>>::new());
    assert_eq!(
        result.terminator,
        ReplayTerminator::CorruptAt {
            offset: WAL_HEADER_LEN,
            reason: CorruptionReason::FirstSequenceMismatch {
                expected: LogSeq::new(5_000),
                actual: LogSeq::new(5_001),
                location: CorruptionLocation::Tail,
            },
        }
    );
}

#[test]
fn wal_record_errors_display_every_value_in_its_role() {
    let cases = [
        (
            RecordError::HeaderTruncated {
                needed: 14,
                available: 3,
            }
            .to_string(),
            "WAL header needs 14 bytes, got 3",
        ),
        (
            RecordError::LengthOverflow {
                payload_length: u32::MAX,
            }
            .to_string(),
            "WAL payload length 4294967295 overflows framing",
        ),
        (
            RecordError::BodyTruncated {
                payload_length: 3,
                needed: 25,
                available: 22,
            }
            .to_string(),
            "WAL payload length 3 needs 25 framed bytes, got 22",
        ),
        (
            RecordError::ChecksumMismatch {
                expected: 0x0123_4567_89ab_cdef,
                actual: 0xfedc_ba98_7654_3210,
                record_length: 25,
            }
            .to_string(),
            "WAL checksum expected 0x0123456789abcdef, computed 0xfedcba9876543210",
        ),
    ];
    for (actual, expected) in cases {
        assert_eq!(actual, expected);
    }
    assert_eq!(
        RecordEncodeError::PayloadTooLarge(4_294_967_296).to_string(),
        "WAL payload length 4294967296 exceeds u32"
    );
}

#[test]
fn wal_header_errors_display_typed_values() {
    let cases = [
        (WalHeaderError::Missing, "WAL file header is missing"),
        (
            WalHeaderError::Truncated {
                needed: 40,
                available: 7,
            },
            "WAL file header needs 40 bytes, got 7",
        ),
        (
            WalHeaderError::WrongMagic {
                expected: *b"ZEPEMBED",
                actual: *b"NOPEFILE",
            },
            "WAL magic expected [90, 69, 80, 69, 77, 66, 69, 68], got [78, 79, 80, 69, 70, 73, 76, 69]",
        ),
        (
            WalHeaderError::WrongFamily {
                expected: 11,
                actual: 10,
            },
            "WAL family expected 11, got 10",
        ),
        (
            WalHeaderError::UnsupportedVersion {
                family: 11,
                version: 2,
                minimum: 1,
                maximum: 1,
            },
            "WAL family 11 version 2 is outside accepted range 1..=1",
        ),
        (
            WalHeaderError::InvalidHeaderLength {
                expected: 40,
                actual: 41,
            },
            "WAL header length expected 40, got 41",
        ),
        (
            WalHeaderError::NonZeroFileLength { actual: 9 },
            "WAL file length must be zero, got 9",
        ),
        (
            WalHeaderError::InvalidSharedHeader {
                check: FormatCheck::FileLength,
            },
            "WAL shared header failed FileLength",
        ),
    ];
    for (error, expected) in cases {
        assert_eq!(error.to_string(), expected);
    }
}

#[test]
fn wal_record_round_trip_checks_header_and_payload_bytes() {
    let original = encoded(7, 0x1234, b"payload");
    let decoded = decode_record(&original).expect("valid record");
    assert_eq!(decoded.record.seq, LogSeq::new(7));
    assert_eq!(decoded.record.op, 0x1234);
    assert_eq!(decoded.record.payload, b"payload");
    assert_eq!(decoded.encoded_len, original.len());

    for (label, offset) in [
        ("length", 0_usize),
        ("sequence", 4),
        ("operation", 12),
        ("payload", 14),
    ] {
        let mut damaged = original.clone();
        damaged[offset] ^= 1;
        let payload_length = u32::from_le_bytes(damaged[..4].try_into().unwrap()) as usize;
        let checksum_offset = 14 + payload_length;
        let checksum_end = checksum_offset + 8;
        let expected =
            u64::from_le_bytes(damaged[checksum_offset..checksum_end].try_into().unwrap());
        let actual = xxh3_64(&damaged[..checksum_offset]);
        let decoded = decode_record(&damaged);
        assert_eq!(
            decoded,
            Err(RecordError::ChecksumMismatch {
                expected,
                actual,
                record_length: checksum_end,
            }),
            "{label} mutation"
        );
    }

    let exact = encoded(8, 9, &[0x5a; 256]);
    assert_eq!(u32::from_le_bytes(exact[..4].try_into().unwrap()), 256);
    assert_eq!(
        decode_record(&exact).expect("exact bound").encoded_len,
        exact.len()
    );
}

#[test]
fn wal_replay_empty_and_valid_records_end_cleanly() {
    let empty_bytes = wal_bytes(1, &[]);
    let empty = replay(&empty_bytes);
    assert_eq!(empty.records, Vec::<WalRecord<'_>>::new());
    assert_eq!(empty.terminator, ReplayTerminator::CleanEnd);

    let bytes = wal_bytes(1, &[(1, 10, b"one"), (2, 20, b"two")]);
    let result = replay(&bytes);
    assert_eq!(
        result.records,
        vec![
            WalRecord {
                seq: LogSeq::new(1),
                op: 10,
                payload: b"one",
            },
            WalRecord {
                seq: LogSeq::new(2),
                op: 20,
                payload: b"two",
            },
        ]
    );
    assert_eq!(result.terminator, ReplayTerminator::CleanEnd);
}

#[derive(Clone, Copy, Debug)]
enum TailDamage {
    Header,
    Body,
    Checksum,
}

#[test]
fn wal_replay_tail_corruption_returns_valid_prefix_and_specific_reason() {
    let first = wal_bytes(1, &[(1, 10, b"one")]);
    let second = encoded(2, 20, b"two");
    let cases = [
        (
            TailDamage::Header,
            3_usize,
            RecordError::HeaderTruncated {
                needed: 14,
                available: 3,
            },
        ),
        (
            TailDamage::Body,
            MIN_RECORD_LEN,
            RecordError::BodyTruncated {
                payload_length: 3,
                needed: MIN_RECORD_LEN + 3,
                available: MIN_RECORD_LEN,
            },
        ),
    ];
    for (damage, retained, expected_error) in cases {
        let mut bytes = first.clone();
        bytes.extend_from_slice(&second[..retained]);
        let result = replay(&bytes);
        assert_eq!(result.records.len(), 1, "{damage:?}");
        assert_eq!(
            result.terminator,
            ReplayTerminator::CorruptAt {
                offset: first.len(),
                reason: CorruptionReason::Record {
                    location: CorruptionLocation::Tail,
                    error: expected_error,
                },
            },
            "{damage:?}"
        );
    }

    let mut checksum = first.clone();
    let mut damaged = second.clone();
    damaged[14] ^= 1;
    checksum.extend_from_slice(&damaged);
    let result = replay(&checksum);
    assert_eq!(result.records.len(), 1, "{:?}", TailDamage::Checksum);
    assert!(
        matches!(
            result.terminator,
            ReplayTerminator::CorruptAt {
                offset,
                reason: CorruptionReason::Record {
                    location: CorruptionLocation::Tail,
                    error: RecordError::ChecksumMismatch { .. },
                },
            } if offset == first.len()
        ),
        "checksum tail returned {:?}",
        result.terminator
    );
}

#[test]
fn wal_replay_middle_corruption_is_not_treated_as_a_torn_tail() {
    let first = wal_bytes(1, &[(1, 10, b"one")]);
    let mut second = encoded(2, 20, b"two");
    second[14] ^= 1;
    let third = encoded(3, 30, b"three");
    let mut bytes = first.clone();
    bytes.extend_from_slice(&second);
    bytes.extend_from_slice(&third);

    let result = replay(&bytes);
    assert_eq!(result.records.len(), 1);
    assert!(
        matches!(
            result.terminator,
            ReplayTerminator::CorruptAt {
                offset,
                reason: CorruptionReason::Record {
                    location: CorruptionLocation::Middle,
                    error: RecordError::ChecksumMismatch { .. },
                },
            } if offset == first.len()
        ),
        "middle corruption returned {:?}",
        result.terminator
    );
}

#[test]
fn wal_replay_reports_sequence_gap_and_regression() {
    let cases = [
        (
            "gap",
            vec![(1_u64, b"one".as_slice()), (3, b"three".as_slice())],
            CorruptionReason::SequenceGap {
                expected: LogSeq::new(2),
                actual: LogSeq::new(3),
                location: CorruptionLocation::Tail,
            },
        ),
        (
            "regression",
            vec![(1_u64, b"one".as_slice()), (1, b"again".as_slice())],
            CorruptionReason::SequenceRegression {
                expected: LogSeq::new(2),
                actual: LogSeq::new(1),
                location: CorruptionLocation::Tail,
            },
        ),
    ];
    for (label, records, expected_reason) in cases {
        let records = records
            .into_iter()
            .map(|(seq, payload)| (seq, 1, payload))
            .collect::<Vec<_>>();
        let bytes = wal_bytes(1, &records);
        let result = replay(&bytes);
        assert_eq!(result.records.len(), 1, "{label}");
        assert_eq!(
            result.terminator,
            ReplayTerminator::CorruptAt {
                offset: WAL_HEADER_LEN + encoded(1, 1, b"one").len(),
                reason: expected_reason,
            },
            "{label}"
        );
    }
}

#[test]
fn wal_goldens_are_byte_exact_and_replay_semantically() {
    let empty = fixture(include_str!("fixtures/format/wal_empty_v1.hex"));
    assert_eq!(empty, wal_bytes(1, &[]));
    let replayed = replay(&empty);
    assert_eq!(replayed.records, Vec::<WalRecord<'_>>::new());
    assert_eq!(replayed.terminator, ReplayTerminator::CleanEnd);

    let single = wal_bytes(1, &[(1, 7, b"abc")]);
    assert_eq!(
        single,
        fixture(include_str!("fixtures/format/wal_single_v1.hex"))
    );
    let replayed = replay(&single);
    assert_eq!(
        replayed.records,
        vec![WalRecord {
            seq: LogSeq::new(1),
            op: 7,
            payload: b"abc",
        }]
    );
    assert_eq!(replayed.terminator, ReplayTerminator::CleanEnd);

    let mut multi = wal_bytes(1, &[(1, 1, b"")]);
    multi.extend_from_slice(&encoded(2, 2, &[0x00, 0xff, 0x10]));
    multi.extend_from_slice(&encoded(3, u16::MAX, b"wal"));
    assert_eq!(
        multi,
        fixture(include_str!("fixtures/format/wal_multi_v1.hex"))
    );
    let replayed = replay(&multi);
    assert_eq!(replayed.records.len(), 3);
    assert_eq!(replayed.terminator, ReplayTerminator::CleanEnd);

    let torn = fixture(include_str!("fixtures/format/wal_torn_tail_v1.hex"));
    assert_eq!(torn, multi[..multi.len() - 3]);
    let replayed = replay(&torn);
    assert_eq!(replayed.records.len(), 2);
    assert!(matches!(
        replayed.terminator,
        ReplayTerminator::CorruptAt {
            reason: CorruptionReason::Record {
                location: CorruptionLocation::Tail,
                error: RecordError::BodyTruncated { .. },
            },
            ..
        }
    ));

    let expected_header = encode_header(LogSeq::new(1)).expect("registered WAL header");
    for bytes in [&empty, &single, &multi, &torn] {
        assert_eq!(&bytes[..WAL_HEADER_LEN], expected_header);
    }
}
