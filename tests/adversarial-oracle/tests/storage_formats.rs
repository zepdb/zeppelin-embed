use zeppelin_embed_adversarial_oracle::storage_durability::*;

fn hex(text: &str) -> Vec<u8> {
    let digits: Vec<_> = text
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    digits
        .chunks_exact(2)
        .map(|pair| {
            ((char::from(pair[0]).to_digit(16).unwrap() << 4)
                | char::from(pair[1]).to_digit(16).unwrap()) as u8
        })
        .collect()
}

fn manifest() -> Vec<u8> {
    hex(include_str!(
        "../../../crates/zeppelin-embed/tests/fixtures/format/manifest_v2.hex"
    ))
}

fn put16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}
fn put32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
fn put64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}
fn file_checksum(bytes: &mut [u8]) {
    let end = bytes.len() - 8;
    let checksum = xxh3_64(&bytes[..end]);
    put64(bytes, end, checksum);
}
fn assert_manifest_error(bytes: &[u8], check: ParseCheck, offset: u64) {
    let error = parse_manifest("manifest.ze", bytes).unwrap_err();
    assert_eq!((error.check, error.offset), (check, offset), "{error}");
    assert_eq!(error.artifact, "manifest.ze");
    assert!(error.to_string().starts_with("manifest.ze at "));
}

#[test]
fn manifest_header_rejects_each_malformed_framing_field() {
    let valid = manifest();
    let parsed = parse_manifest("manifest.ze", &valid).unwrap();
    assert_eq!((parsed.generation, parsed.log_seq), (21, 13));
    assert_manifest_error(&valid[..31], ParseCheck::Length, 0);
    for (offset, value, check) in [
        (0, 0, ParseCheck::Magic),
        (8, 2, ParseCheck::Family),
        (10, 1, ParseCheck::Version),
        (12, 1, ParseCheck::Flags),
        (16, 31, ParseCheck::HeaderLength),
        (24, 0, ParseCheck::FileLength),
    ] {
        let mut invalid = valid.clone();
        invalid[offset] = value;
        assert_manifest_error(&invalid, check, offset as u64);
    }
    let mut invalid = valid.clone();
    invalid[40] ^= 1;
    assert_manifest_error(&invalid, ParseCheck::FileChecksum, (valid.len() - 8) as u64);
    file_checksum(&mut invalid);
    assert_manifest_error(
        &invalid,
        ParseCheck::BlockChecksum,
        (valid.len() - 16) as u64,
    );
    let mut invalid = valid.clone();
    put64(&mut invalid, 32, u64::MAX);
    file_checksum(&mut invalid);
    assert_manifest_error(&invalid, ParseCheck::Bounds, 40);
    let mut invalid = valid.clone();
    put64(&mut invalid, 32, 0);
    file_checksum(&mut invalid);
    assert_manifest_error(&invalid, ParseCheck::TrailingBytes, 48);
    assert_eq!(parse_manifest("manifest.ze", &valid).unwrap(), parsed);
}

fn framed_manifest(payload: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0; 40];
    bytes[..8].copy_from_slice(b"ZEPEMBED");
    put16(&mut bytes, 8, 10);
    put16(&mut bytes, 10, 2);
    put64(&mut bytes, 16, 32);
    put64(&mut bytes, 24, (payload.len() + 56) as u64);
    put64(&mut bytes, 32, payload.len() as u64);
    bytes.extend_from_slice(payload);
    bytes.extend_from_slice(&xxh3_64(payload).to_le_bytes());
    bytes.extend_from_slice(&[0; 8]);
    file_checksum(&mut bytes);
    bytes
}

#[test]
fn manifest_payload_validates_identity_schema_epoch_and_range_boundaries() {
    let mut empty = vec![0; 56];
    put64(&mut empty, 0, 9);
    for end in 0..56 {
        let error = parse_manifest("manifest.ze", &framed_manifest(&empty[..end])).unwrap_err();
        assert_eq!(
            error.check,
            ParseCheck::Length,
            "payload length {end}: {error}"
        );
    }
    let parsed = parse_manifest("manifest.ze", &framed_manifest(&empty)).unwrap();
    assert_eq!(parsed.generation, 9);
    assert!(parsed.segments.is_empty());
    for (offset, value, expected_offset) in [(28, 1, 28), (33, 1, 33), (40, 1, 32), (32, 2, 32)] {
        let mut payload = empty.clone();
        payload[offset] = value;
        assert_manifest_error(
            &framed_manifest(&payload),
            ParseCheck::Reserved,
            expected_offset,
        );
    }

    let mut segment = empty.clone();
    put32(&mut segment, 16, 1);
    segment.extend_from_slice(&[0; 52]);
    segment[56] = 7;
    put32(&mut segment, 72, 3);
    put16(&mut segment, 76, 4);
    put32(&mut segment, 80, 2);
    put64(&mut segment, 84, 4096);
    let parsed = parse_manifest("manifest.ze", &framed_manifest(&segment)).unwrap();
    assert_eq!((parsed.segments[0].rows, parsed.segments[0].dims), (3, 2));
    for (offset, value, check) in [(76, 3, ParseCheck::Version), (78, 1, ParseCheck::Reserved)] {
        let mut payload = segment.clone();
        payload[offset] = value;
        assert_manifest_error(&framed_manifest(&payload), check, offset as u64);
    }
    let mut ranges = segment.clone();
    ranges.extend_from_slice(b"TSR1");
    ranges.extend_from_slice(&1u32.to_le_bytes());
    ranges.extend_from_slice(&[0; 24]);
    parse_manifest("manifest.ze", &framed_manifest(&ranges)).unwrap();
    for (offset, value, check, expected_offset) in [
        (108, b'X', ParseCheck::TrailingBytes, 108),
        (112, 0, ParseCheck::Ordering, 112),
        (116, 3, ParseCheck::Bounds, 116),
        (117, 1, ParseCheck::Reserved, 117),
        (124, 1, ParseCheck::Bounds, 116),
    ] {
        let mut payload = ranges.clone();
        payload[offset] = value;
        assert_manifest_error(&framed_manifest(&payload), check, expected_offset);
    }
    ranges[116] = 2;
    put64(&mut ranges, 124, (-3i64) as u64);
    put64(&mut ranges, 132, 8);
    parse_manifest("manifest.ze", &framed_manifest(&ranges)).unwrap();
    put64(&mut ranges, 124, 9);
    assert_manifest_error(&framed_manifest(&ranges), ParseCheck::Bounds, 116);
    ranges[116] = 0;
    put64(&mut ranges, 124, 0);
    put64(&mut ranges, 132, 0);
    ranges.push(0);
    assert_manifest_error(&framed_manifest(&ranges), ParseCheck::TrailingBytes, 140);

    let mut schema = empty.clone();
    put32(&mut schema, 24, 1);
    schema.extend_from_slice(&[0; 12]);
    put32(&mut schema, 64, 1);
    schema.push(b'x');
    parse_manifest("manifest.ze", &framed_manifest(&schema)).unwrap();
    schema[62] = 2;
    assert_manifest_error(&framed_manifest(&schema), ParseCheck::Reserved, 62);
    schema[62] = 0;
    schema[68] = 255;
    assert_manifest_error(&framed_manifest(&schema), ParseCheck::Bounds, 68);

    let mut epoch = empty;
    put32(&mut epoch, 20, 1);
    epoch.extend_from_slice(&[0; 16]);
    // Each minimal tower has three empty strings, dims/normalization, an empty
    // prompt, max tokens/runtime/compute units, and an absent OS-build tag.
    epoch.extend_from_slice(&[0; 34]);
    epoch.extend_from_slice(&[0; 34]);
    epoch.extend_from_slice(&[0; 4]);
    parse_manifest("manifest.ze", &framed_manifest(&epoch)).unwrap();
    epoch[102] = 2;
    assert_manifest_error(&framed_manifest(&epoch), ParseCheck::Reserved, 102);
    epoch[102] = 1;
    epoch.splice(106..106, [0, 0, 0, 0]);
    parse_manifest("manifest.ze", &framed_manifest(&epoch)).unwrap();
    epoch[103] = 1;
    assert_manifest_error(&framed_manifest(&epoch), ParseCheck::Reserved, 103);
}

fn segment() -> Vec<u8> {
    let mut bytes = vec![0; 16_384 + 3 + 8];
    bytes[..8].copy_from_slice(b"ZEPEMBED");
    put16(&mut bytes, 8, 2);
    put16(&mut bytes, 10, 1);
    put64(&mut bytes, 16, 104);
    let length = bytes.len();
    put64(&mut bytes, 24, length as u64);
    bytes[32] = 7;
    put32(&mut bytes, 48, 3);
    put16(&mut bytes, 52, 1);
    put16(&mut bytes, 56, 4);
    put32(&mut bytes, 60, 2);
    put16(&mut bytes, 64, 1);
    put16(&mut bytes, 66, 1);
    put64(&mut bytes, 72, 16_384);
    put64(&mut bytes, 80, 3);
    bytes[16_384..16_387].copy_from_slice(b"abc");
    put64(&mut bytes, 88, xxh3_64(b"abc"));
    segment_checksums(&mut bytes);
    bytes
}
fn segment_checksums(bytes: &mut [u8]) {
    let checksum = xxh3_64(&bytes[..96]);
    put64(bytes, 96, checksum);
    file_checksum(bytes);
}

#[test]
fn segment_directory_rejects_checksums_reserved_bytes_and_invalid_ranges() {
    let valid = segment();
    let parsed = parse_segment("segment.zseg", &valid).unwrap();
    assert_eq!(
        (parsed.fact.rows, parsed.fact.dims, parsed.regions.len()),
        (3, 2, 1)
    );
    for (offset, value, check, error_offset) in [
        (24, 0, ParseCheck::FileLength, 24),
        (54, 1, ParseCheck::Reserved, 54),
        (58, 1, ParseCheck::Reserved, 54),
        (56, 3, ParseCheck::Version, 56),
        (16, 0, ParseCheck::HeaderLength, 16),
        (96, 0, ParseCheck::BlockChecksum, 96),
    ] {
        let mut invalid = valid.clone();
        invalid[offset] = value;
        let error = parse_segment("segment.zseg", &invalid).unwrap_err();
        assert_eq!(
            (error.check, error.offset),
            (check, error_offset),
            "{error}"
        );
    }
    let mut invalid = valid.clone();
    invalid[16_384] ^= 1;
    assert_eq!(
        parse_segment("segment.zseg", &invalid).unwrap_err().check,
        ParseCheck::FileChecksum
    );
    file_checksum(&mut invalid);
    let error = parse_segment("segment.zseg", &invalid).unwrap_err();
    assert_eq!((error.check, error.offset), (ParseCheck::BlockChecksum, 88));
    for (offset, value, check, error_offset) in [
        (68, 1, ParseCheck::Reserved, 68),
        (72, 16_385, ParseCheck::Ordering, 72),
        (72, 0, ParseCheck::Ordering, 72),
        (80, 4, ParseCheck::Ordering, 72),
        (72, u64::MAX, ParseCheck::Bounds, 72),
    ] {
        let mut invalid = valid.clone();
        put64(&mut invalid, offset, value);
        segment_checksums(&mut invalid);
        let error = parse_segment("segment.zseg", &invalid).unwrap_err();
        assert_eq!(
            (error.check, error.offset),
            (check, error_offset),
            "{error}"
        );
    }
    assert_eq!(parse_segment("segment.zseg", &valid).unwrap(), parsed);
}

fn wal_header(first: u64) -> Vec<u8> {
    let mut bytes = vec![0; 40];
    bytes[..8].copy_from_slice(b"ZEPEMBED");
    put16(&mut bytes, 8, 11);
    put16(&mut bytes, 10, 1);
    put64(&mut bytes, 16, 40);
    put64(&mut bytes, 32, first);
    bytes
}
fn wal_record(sequence: u64) -> Vec<u8> {
    let mut bytes = vec![0; 15];
    put32(&mut bytes, 0, 1);
    put64(&mut bytes, 4, sequence);
    put16(&mut bytes, 12, 7);
    bytes[14] = 42;
    let checksum = xxh3_64(&bytes);
    bytes.extend_from_slice(&checksum.to_le_bytes());
    bytes
}

#[test]
fn wal_parser_preserves_exact_header_and_corrupt_prefix_diagnostics() {
    let header = wal_header(9);
    assert_eq!(
        parse_wal("wal.ze", &header).unwrap().terminator,
        WalTerminator::CleanEnd
    );
    for (length, reason) in [
        (0, WalHeaderFailure::Missing),
        (
            39,
            WalHeaderFailure::Truncated {
                needed: 40,
                available: 39,
            },
        ),
    ] {
        assert_eq!(
            parse_wal("wal.ze", &header[..length]).unwrap().terminator,
            WalTerminator::InvalidHeader {
                artifact: "wal.ze".into(),
                reason
            }
        );
    }
    for (offset, value, reason) in [
        (
            0,
            b'X',
            WalHeaderFailure::WrongMagic {
                expected: *b"ZEPEMBED",
                actual: *b"XEPEMBED",
            },
        ),
        (
            8,
            2,
            WalHeaderFailure::WrongFamily {
                expected: 11,
                actual: 2,
            },
        ),
        (
            10,
            2,
            WalHeaderFailure::UnsupportedVersion {
                family: 11,
                version: 2,
                minimum: 1,
                maximum: 1,
            },
        ),
        (12, 1, WalHeaderFailure::NonZeroFlags { actual: 1 }),
        (
            16,
            32,
            WalHeaderFailure::InvalidHeaderLength {
                expected: 40,
                actual: 32,
            },
        ),
        (24, 1, WalHeaderFailure::NonZeroFileLength { actual: 1 }),
    ] {
        let mut bytes = header.clone();
        bytes[offset] = value;
        assert_eq!(
            parse_wal("wal.ze", &bytes).unwrap().terminator,
            WalTerminator::InvalidHeader {
                artifact: "wal.ze".into(),
                reason
            }
        );
    }
    let first = wal_record(9);
    for length in 1..23 {
        let mut bytes = header.clone();
        bytes.extend_from_slice(&first[..length]);
        let reason = if length < 14 {
            WalRecordFailure::HeaderTruncated {
                needed: 14,
                available: length as u64,
            }
        } else {
            WalRecordFailure::BodyTruncated {
                payload_length: 1,
                needed: 23,
                available: length as u64,
            }
        };
        assert_eq!(
            parse_wal("wal.ze", &bytes).unwrap().terminator,
            WalTerminator::CorruptAt {
                artifact: "wal.ze".into(),
                offset: 40,
                location: CorruptionLocation::Tail,
                reason
            }
        );
    }
    let mut clean = header.clone();
    clean.extend_from_slice(&first);
    clean.extend_from_slice(&wal_record(10));
    let parsed = parse_wal("wal.ze", &clean).unwrap();
    assert_eq!(
        parsed.records,
        vec![
            WalRecordFact {
                seq: 9,
                op: 7,
                payload: vec![42]
            },
            WalRecordFact {
                seq: 10,
                op: 7,
                payload: vec![42]
            }
        ]
    );
    for (offset, location, retained) in [
        (40, CorruptionLocation::Middle, 0),
        (63, CorruptionLocation::Tail, 1),
    ] {
        let mut bytes = clean.clone();
        bytes[offset + 14] ^= 1;
        let parsed = parse_wal("wal.ze", &bytes).unwrap();
        assert_eq!(parsed.records.len(), retained);
        let WalTerminator::CorruptAt {
            offset: actual,
            location: actual_location,
            reason:
                WalRecordFailure::ChecksumMismatch {
                    expected,
                    actual: computed,
                    record_length,
                },
            ..
        } = parsed.terminator
        else {
            panic!("wrong checksum diagnostic")
        };
        assert_eq!(
            (actual, actual_location, record_length),
            (offset as u64, location, 23)
        );
        assert_ne!(expected, computed);
    }
    let mut wrong = header;
    wrong.extend_from_slice(&wal_record(10));
    assert_eq!(
        parse_wal("wal.ze", &wrong).unwrap().terminator,
        WalTerminator::CorruptAt {
            artifact: "wal.ze".into(),
            offset: 40,
            location: CorruptionLocation::Tail,
            reason: WalRecordFailure::Sequence {
                expected: 9,
                actual: 10
            }
        }
    );
    let mut overflow = wal_header(u64::MAX);
    overflow.extend_from_slice(&wal_record(u64::MAX));
    assert_eq!(
        parse_wal("wal.ze", &overflow).unwrap().terminator,
        WalTerminator::CleanEnd
    );
    overflow.extend_from_slice(&wal_record(0));
    assert_eq!(
        parse_wal("wal.ze", &overflow).unwrap().terminator,
        WalTerminator::CorruptAt {
            artifact: "wal.ze".into(),
            offset: 63,
            location: CorruptionLocation::Tail,
            reason: WalRecordFailure::SequenceOverflow { previous: u64::MAX }
        }
    );
}
