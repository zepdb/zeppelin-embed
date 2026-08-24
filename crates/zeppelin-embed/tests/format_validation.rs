#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

use xxhash_rust::xxh3::xxh3_64;
use zeppelin_embed::format::frame::{
    FILE_HEADER_LEN, FILE_TRAILER_LEN, FormatCheck, decode_artifact, decode_header, encode_artifact,
};
use zeppelin_embed::format::golden::{HexError, decode_hex};
use zeppelin_embed::format::{FormatFamily, FormatRegistry, RegistryError};
use zeppelin_embed::manifest::{
    EpochMeta, Manifest, ManifestError, decode_manifest, encode_manifest,
};
use zeppelin_embed::meta::{ColumnDefinition, ColumnId, ColumnType, Schema};
use zeppelin_embed::segment::{ClusteringKeyRange, SegmentId, SegmentMeta};

fn rewrite_file_checksum(bytes: &mut [u8]) {
    let trailer = bytes.len() - FILE_TRAILER_LEN;
    let checksum = xxh3_64(&bytes[..trailer]).to_le_bytes();
    bytes[trailer..].copy_from_slice(&checksum);
}

fn rewrite_artifact_checksums(bytes: &mut [u8]) {
    let payload_length = u64::from_le_bytes(
        bytes[FILE_HEADER_LEN..FILE_HEADER_LEN + 8]
            .try_into()
            .unwrap(),
    ) as usize;
    let payload_start = FILE_HEADER_LEN + 8;
    let payload_end = payload_start + payload_length;
    let block = xxh3_64(&bytes[payload_start..payload_end]).to_le_bytes();
    bytes[payload_end..payload_end + 8].copy_from_slice(&block);
    rewrite_file_checksum(bytes);
}

#[test]
fn format_registry_and_hex_errors_are_typed() {
    assert_eq!(decode_hex("Aa 01\n"), Ok(vec![0xaa, 1]));
    assert_eq!(decode_hex("0"), Err(HexError::OddLength));
    assert_eq!(decode_hex("0g"), Err(HexError::InvalidDigit(b'g')));
    assert!(HexError::OddLength.to_string().contains("odd"));
    assert!(HexError::InvalidDigit(b'g').to_string().contains("103"));

    assert_eq!(
        FormatRegistry::require(999, 1),
        Err(RegistryError::UnknownFamily(999))
    );
    assert!(matches!(
        FormatRegistry::require(FormatFamily::Frame.id(), 2),
        Err(RegistryError::UnsupportedVersion {
            family: 1,
            version: 2,
            minimum: 1,
            maximum: 1
        })
    ));
    assert_eq!(
        FormatRegistry::require_scheme(6),
        Err(RegistryError::UnknownScheme(6))
    );
    for error in [
        RegistryError::UnknownFamily(9),
        RegistryError::UnsupportedVersion {
            family: 1,
            version: 0,
            minimum: 1,
            maximum: 1,
        },
        RegistryError::RetiredScheme(3),
        RegistryError::UnknownScheme(7),
    ] {
        assert!(!error.to_string().is_empty());
    }
}

#[test]
fn frame_decoder_names_every_failed_check() {
    let valid = encode_artifact(FormatFamily::Frame, 7, b"payload");
    let header = decode_header("frame", FormatFamily::Frame, &valid).expect("header");
    assert_eq!(header.flags, 7);

    let mut cases = Vec::new();
    cases.push(("short", valid[..12].to_vec(), FormatCheck::Length));
    let mut magic = valid.clone();
    magic[0] ^= 1;
    cases.push(("magic", magic, FormatCheck::Magic));
    let mut family = valid.clone();
    family[8..10].copy_from_slice(&FormatFamily::Manifest.id().to_le_bytes());
    rewrite_file_checksum(&mut family);
    cases.push(("family", family, FormatCheck::Family));
    let mut version = valid.clone();
    version[10..12].copy_from_slice(&2_u16.to_le_bytes());
    rewrite_file_checksum(&mut version);
    cases.push(("version", version, FormatCheck::Version));
    let mut header_low = valid.clone();
    header_low[16..24].copy_from_slice(&31_u64.to_le_bytes());
    rewrite_file_checksum(&mut header_low);
    cases.push(("header-low", header_low, FormatCheck::HeaderLength));
    let mut header_extra = valid.clone();
    header_extra[16..24].copy_from_slice(&33_u64.to_le_bytes());
    rewrite_file_checksum(&mut header_extra);
    cases.push(("header-extra", header_extra, FormatCheck::HeaderLength));
    let mut file_length = valid.clone();
    file_length[24..32].copy_from_slice(&1_u64.to_le_bytes());
    cases.push(("file-length", file_length, FormatCheck::FileLength));
    let mut file_checksum = valid.clone();
    let last = file_checksum.len() - 1;
    file_checksum[last] ^= 1;
    cases.push(("file-checksum", file_checksum, FormatCheck::FileChecksum));
    let mut block_length = valid.clone();
    block_length[32..40].copy_from_slice(&0_u64.to_le_bytes());
    rewrite_file_checksum(&mut block_length);
    cases.push(("block-length", block_length, FormatCheck::BlockLength));
    let mut block_checksum = valid.clone();
    let checksum_offset = FILE_HEADER_LEN + 8 + b"payload".len();
    block_checksum[checksum_offset] ^= 1;
    rewrite_file_checksum(&mut block_checksum);
    cases.push(("block-checksum", block_checksum, FormatCheck::BlockChecksum));

    for (artifact, bytes, expected_check) in cases {
        let error = decode_artifact(artifact, FormatFamily::Frame, &bytes).expect_err("damage");
        assert_eq!(error.artifact(), artifact);
        assert_eq!(error.check(), expected_check, "{}", error.detail());
        assert!(error.to_string().contains(artifact));
    }
}

fn payload_artifact(payload: &[u8]) -> Vec<u8> {
    encode_artifact(FormatFamily::Manifest, 0, payload)
}

fn minimal_payload() -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&1_u64.to_le_bytes());
    payload.extend_from_slice(&2_u64.to_le_bytes());
    payload.extend_from_slice(&0_u32.to_le_bytes());
    payload.extend_from_slice(&0_u32.to_le_bytes());
    payload.extend_from_slice(&0_u32.to_le_bytes());
    payload.extend_from_slice(&0_u32.to_le_bytes());
    payload
}

#[test]
fn manifest_codec_roundtrips_all_fields_and_rejects_payload_shapes() {
    let schema = Schema::new(vec![
        ColumnDefinition::new(ColumnId::new(1), "u", ColumnType::U64, true),
        ColumnDefinition::new(ColumnId::new(2), "i", ColumnType::I64, false),
        ColumnDefinition::new(ColumnId::new(3), "f", ColumnType::F64, true),
        ColumnDefinition::new(ColumnId::new(4), "b", ColumnType::Bool, true),
        ColumnDefinition::new(ColumnId::new(5), "d", ColumnType::DictionaryString, true),
        ColumnDefinition::new(ColumnId::new(6), "r", ColumnType::RawString, true),
    ])
    .expect("schema");
    let manifest = Manifest {
        generation: 8,
        log_seq: 7,
        segments: vec![SegmentMeta {
            id: SegmentId::new(1, [2; 10]),
            row_count: 3,
            scheme: 4,
            dims: 65,
            file_size: 99,
            clustering_key_range: zeppelin_embed::segment::ClusteringKeyRange::Unstamped,
        }],
        epochs: vec![EpochMeta {
            id: 1,
            model: "model".to_owned(),
            tokenizer: "tok".to_owned(),
        }],
        schema,
    };
    let encoded = encode_manifest(&manifest).expect("encode");
    assert_eq!(
        decode_manifest("manifest", &encoded).expect("decode"),
        manifest
    );

    let mut reserved = minimal_payload();
    reserved[28..32].copy_from_slice(&1_u32.to_le_bytes());
    assert!(matches!(
        decode_manifest("reserved", &payload_artifact(&reserved)),
        Err(ManifestError::Decode(_))
    ));

    let mut truncated_segment = minimal_payload();
    truncated_segment[16..20].copy_from_slice(&1_u32.to_le_bytes());
    assert!(decode_manifest("segment", &payload_artifact(&truncated_segment)).is_err());

    let mut bad_scheme = minimal_payload();
    bad_scheme[16..20].copy_from_slice(&1_u32.to_le_bytes());
    bad_scheme.extend_from_slice(&[0_u8; 20]);
    bad_scheme.extend_from_slice(&3_u16.to_le_bytes());
    bad_scheme.extend_from_slice(&[0_u8; 14]);
    assert!(decode_manifest("scheme", &payload_artifact(&bad_scheme)).is_err());

    let mut trailing = minimal_payload();
    trailing.push(1);
    assert!(decode_manifest("trailing", &payload_artifact(&trailing)).is_err());

    let mut checksum_rewrite = encoded.clone();
    checksum_rewrite[40] ^= 1;
    rewrite_artifact_checksums(&mut checksum_rewrite);
    assert!(decode_manifest("semantic", &checksum_rewrite).is_ok());

    let mut segment_reserved = minimal_payload();
    segment_reserved[16..20].copy_from_slice(&1_u32.to_le_bytes());
    segment_reserved.extend_from_slice(&[0_u8; 16]);
    segment_reserved.extend_from_slice(&0_u32.to_le_bytes());
    segment_reserved.extend_from_slice(&4_u16.to_le_bytes());
    segment_reserved.extend_from_slice(&1_u16.to_le_bytes());
    segment_reserved.extend_from_slice(&3_u32.to_le_bytes());
    segment_reserved.extend_from_slice(&0_u64.to_le_bytes());
    assert!(decode_manifest("segment-reserved", &payload_artifact(&segment_reserved)).is_err());

    let mut invalid_utf8 = minimal_payload();
    invalid_utf8[20..24].copy_from_slice(&1_u32.to_le_bytes());
    invalid_utf8.extend_from_slice(&1_u64.to_le_bytes());
    invalid_utf8.extend_from_slice(&1_u32.to_le_bytes());
    invalid_utf8.push(0xff);
    invalid_utf8.extend_from_slice(&0_u32.to_le_bytes());
    assert!(decode_manifest("utf8", &payload_artifact(&invalid_utf8)).is_err());

    for (column_type, nullable, id) in [(99_u16, 0_u16, 1_u32), (1, 2, 1), (1, 0, 0)] {
        let mut bad_schema = minimal_payload();
        bad_schema[24..28].copy_from_slice(&1_u32.to_le_bytes());
        bad_schema.extend_from_slice(&id.to_le_bytes());
        bad_schema.extend_from_slice(&column_type.to_le_bytes());
        bad_schema.extend_from_slice(&nullable.to_le_bytes());
        bad_schema.extend_from_slice(&1_u32.to_le_bytes());
        bad_schema.push(b'x');
        assert!(decode_manifest("schema", &payload_artifact(&bad_schema)).is_err());
    }
}

#[test]
fn manifest_clustering_extension_rejects_each_semantic_corruption() {
    let manifest = Manifest {
        generation: 1,
        log_seq: 0,
        segments: vec![SegmentMeta {
            id: SegmentId::new(1, [0x33; 10]),
            row_count: 1,
            scheme: 4,
            dims: 2,
            file_size: 64,
            clustering_key_range: ClusteringKeyRange::Bounded {
                min_ts: 10,
                max_ts: 20,
            },
        }],
        epochs: Vec::new(),
        schema: Schema::new(Vec::new()).expect("schema"),
    };
    let framed = encode_manifest(&manifest).expect("bounded manifest");
    let payload = decode_artifact("manifest", FormatFamily::Manifest, &framed)
        .expect("manifest frame")
        .payload
        .to_vec();
    let extension = 68_usize;
    assert_eq!(&payload[extension..extension + 4], b"TSR1");

    type Corruption = (&'static str, Box<dyn Fn(&mut Vec<u8>)>);
    let cases: Vec<Corruption> = vec![
        (
            "unknown manifest extension",
            Box::new(move |bytes| bytes[extension] ^= 1),
        ),
        (
            "clustering range count 2",
            Box::new(move |bytes| {
                bytes[extension + 4..extension + 8].copy_from_slice(&2_u32.to_le_bytes());
            }),
        ),
        (
            "reserved bytes are non-zero",
            Box::new(move |bytes| bytes[extension + 9] = 1),
        ),
        (
            "tag 0 requires zero bounds",
            Box::new(move |bytes| bytes[extension + 8] = 0),
        ),
        (
            "range 20..=10 is inverted",
            Box::new(move |bytes| {
                bytes[extension + 16..extension + 24].copy_from_slice(&20_i64.to_le_bytes());
                bytes[extension + 24..extension + 32].copy_from_slice(&10_i64.to_le_bytes());
            }),
        ),
        (
            "unknown clustering range tag 3",
            Box::new(move |bytes| bytes[extension + 8] = 3),
        ),
        (
            "clustering range extension has 23 bytes, expected 24",
            Box::new(|bytes| {
                let _ = bytes.pop();
            }),
        ),
    ];
    for (expected, mutate) in cases {
        let mut damaged = payload.clone();
        mutate(&mut damaged);
        let error = decode_manifest("semantic-extension", &payload_artifact(&damaged))
            .expect_err("semantic extension corruption must fail");
        assert!(error.to_string().contains(expected), "{error}");
    }
}

#[test]
fn persisted_error_types_preserve_sources_and_actionable_values() {
    use std::error::Error as _;

    let io = ManifestError::Io {
        path: "manifest.ze".into(),
        source: std::io::Error::other("disk"),
    };
    assert!(io.to_string().contains("manifest.ze"));
    assert!(io.source().is_some());

    let format = decode_manifest("bad-manifest", &[]).expect_err("bad format");
    assert!(format.to_string().contains("bad-manifest"));
    assert!(format.source().is_some());

    let decode = ManifestError::Decode("field".to_owned());
    assert!(decode.to_string().contains("field"));
    assert!(decode.source().is_none());

    let ahead = ManifestError::AheadOfLog {
        snapshot: 9,
        durable: 8,
    };
    assert!(ahead.to_string().contains("9"));
    assert!(ahead.source().is_none());

    let segment = ManifestError::Segment(zeppelin_embed::segment::SegmentError::Geometry(
        "stride".to_owned(),
    ));
    assert!(segment.to_string().contains("stride"));
    assert!(segment.source().is_some());
}
