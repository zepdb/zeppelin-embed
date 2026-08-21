#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

use std::fs;

use tempfile::tempdir;
use xxhash_rust::xxh3::xxh3_64;
use zeppelin_embed::format::FormatFamily;
use zeppelin_embed::format::frame::{
    FILE_HEADER_LEN, FILE_TRAILER_LEN, decode_artifact, encode_artifact,
};
use zeppelin_embed::manifest::{Manifest, decode_manifest, encode_manifest};
use zeppelin_embed::meta::{AliveSet, ColumnStoreBuilder, Schema};
use zeppelin_embed::segment::SegmentError;
use zeppelin_embed::segment::SegmentId;
use zeppelin_embed::segment::reader::SegmentReader;
use zeppelin_embed::segment::reader::validate_segment_bytes;
use zeppelin_embed::segment::writer::{SegmentBuild, SegmentFactors, encode_segment};

fn empty_segment(id: SegmentId) -> Vec<u8> {
    let columns = ColumnStoreBuilder::new(Schema::new(Vec::new()).expect("schema"))
        .finish()
        .expect("columns");
    let alive = AliveSet::new(0);
    encode_segment(SegmentBuild {
        id,
        scheme: 4,
        dims: 3,
        codes: &[],
        factors: SegmentFactors::Bit4(&[]),
        rescore: &[],
        columns: &columns,
        alive: &alive,
    })
    .expect("segment")
}

fn rewrite_file_checksum(bytes: &mut [u8]) {
    let trailer = bytes.len() - FILE_TRAILER_LEN;
    let checksum = xxh3_64(&bytes[..trailer]).to_le_bytes();
    bytes[trailer..].copy_from_slice(&checksum);
}

fn open_error(path: &std::path::Path, id: SegmentId, context: &str) -> SegmentError {
    match SegmentReader::open(path, id) {
        Ok(_) => panic!("{context}"),
        Err(error) => error,
    }
}

#[test]
fn segment_corruption_matrix_rejects_damage_with_typed_errors() {
    let directory = tempdir().expect("tempdir");
    let id = SegmentId::new(7, [7; 10]);
    let valid = empty_segment(id);
    assert_eq!(validate_segment_bytes(&valid).expect("byte decoder").id, id);
    assert!(validate_segment_bytes(&[]).is_err());
    let valid_path = directory.path().join("valid.zseg");
    fs::write(&valid_path, &valid).expect("valid write");
    let reader = SegmentReader::open(&valid_path, id).expect("valid open");
    reader.validate_all().expect("valid checksum");
    let first = reader.directory().first().copied().expect("first region");

    let damaged_path = directory.path().join("damaged.zseg");
    for boundary in (0..valid.len()).step_by(64) {
        fs::write(&damaged_path, &valid[..boundary]).expect("truncate write");
        let error = open_error(&damaged_path, id, "truncation must fail");
        assert!(error.to_string().contains("damaged.zseg"), "{error}");
    }

    let mut header_flip = valid.clone();
    header_flip[0] ^= 1;
    fs::write(&damaged_path, &header_flip).expect("header write");
    let error = open_error(&damaged_path, id, "header flip must fail");
    assert!(error.to_string().contains("damaged.zseg"), "{error}");

    let body_offset = first.offset as usize;
    let mut body_flip = valid.clone();
    body_flip[body_offset] ^= 1;
    fs::write(&damaged_path, &body_flip).expect("body write");
    let error = SegmentReader::open(&damaged_path, id)
        .expect("header survives")
        .validate_all()
        .expect_err("body flip must fail");
    assert!(error.to_string().contains("region-"), "{error}");

    let mut zero_block = valid.clone();
    zero_block[body_offset..body_offset + first.length as usize].fill(0);
    fs::write(&damaged_path, &zero_block).expect("zero write");
    let error = SegmentReader::open(&damaged_path, id)
        .expect("header survives")
        .validate_all()
        .expect_err("zero block must fail");
    assert!(error.to_string().contains("region-"), "{error}");

    let mut footer_flip = valid.clone();
    let footer = footer_flip.len() - 1;
    footer_flip[footer] ^= 1;
    fs::write(&damaged_path, &footer_flip).expect("footer write");
    let error = SegmentReader::open(&damaged_path, id)
        .expect("header survives")
        .validate_all()
        .expect_err("footer flip must fail");
    assert!(error.to_string().contains("FileChecksum"), "{error}");

    let other_id = SegmentId::new(8, [8; 10]);
    let swapped = empty_segment(other_id);
    fs::write(&damaged_path, &swapped).expect("swap write");
    let error = open_error(&damaged_path, id, "wrong object must fail");
    assert!(error.to_string().contains("object identity"), "{error}");

    let trailer_start = valid.len() - FILE_TRAILER_LEN;
    let mut duplicate = valid[..trailer_start].to_vec();
    duplicate
        .extend_from_slice(&valid[body_offset..body_offset.saturating_add(first.length as usize)]);
    duplicate.extend_from_slice(&valid[trailer_start..]);
    fs::write(&damaged_path, &duplicate).expect("duplicate write");
    let error = open_error(&damaged_path, id, "duplicate block must fail");
    assert!(error.to_string().contains("FileLength"), "{error}");
}

#[test]
fn frame_and_manifest_corruption_matrix_rejects_every_mutation() {
    let frame = encode_artifact(FormatFamily::Frame, 0, &[0x5a; 160]);
    for boundary in (0..frame.len()).step_by(64) {
        let error = decode_artifact("frame.truncated", FormatFamily::Frame, &frame[..boundary])
            .expect_err("truncation must fail");
        assert_eq!(error.artifact(), "frame.truncated");
    }
    let mut header_flip = frame.clone();
    header_flip[0] ^= 1;
    assert!(decode_artifact("frame.header", FormatFamily::Frame, &header_flip).is_err());
    let mut body_flip = frame.clone();
    body_flip[FILE_HEADER_LEN + 12] ^= 1;
    rewrite_file_checksum(&mut body_flip);
    assert!(decode_artifact("frame.body", FormatFamily::Frame, &body_flip).is_err());
    let mut zero_block = frame.clone();
    zero_block[FILE_HEADER_LEN + 8..frame.len() - 16].fill(0);
    rewrite_file_checksum(&mut zero_block);
    assert!(decode_artifact("frame.zero", FormatFamily::Frame, &zero_block).is_err());
    let mut footer_flip = frame.clone();
    let footer = footer_flip.len() - 1;
    footer_flip[footer] ^= 1;
    assert!(decode_artifact("frame.footer", FormatFamily::Frame, &footer_flip).is_err());
    let mut duplicate = frame[..frame.len() - 8].to_vec();
    duplicate.extend_from_slice(&frame[FILE_HEADER_LEN..frame.len() - 8]);
    duplicate.extend_from_slice(&0_u64.to_le_bytes());
    rewrite_file_checksum(&mut duplicate);
    assert!(decode_artifact("frame.duplicate", FormatFamily::Frame, &duplicate).is_err());

    let manifest = Manifest {
        generation: 1,
        log_seq: 1,
        segments: Vec::new(),
        epochs: Vec::new(),
        schema: Schema::new(Vec::new()).expect("schema"),
    };
    let encoded_manifest = encode_manifest(&manifest).expect("manifest");
    let segment = empty_segment(SegmentId::new(8, [8; 10]));
    assert!(decode_manifest("manifest.swap", &segment).is_err());
    for boundary in (0..encoded_manifest.len()).step_by(64) {
        assert!(decode_manifest("manifest.truncated", &encoded_manifest[..boundary]).is_err());
    }
    let mut manifest_flip = encoded_manifest.clone();
    manifest_flip[FILE_HEADER_LEN + 8] ^= 1;
    assert!(decode_manifest("manifest.body", &manifest_flip).is_err());
    let mut manifest_zero = encoded_manifest.clone();
    manifest_zero[FILE_HEADER_LEN + 8..encoded_manifest.len() - 16].fill(0);
    assert!(decode_manifest("manifest.zero", &manifest_zero).is_err());
    let mut manifest_footer = encoded_manifest.clone();
    let footer = manifest_footer.len() - 1;
    manifest_footer[footer] ^= 1;
    assert!(decode_manifest("manifest.footer", &manifest_footer).is_err());
    let mut manifest_duplicate = encoded_manifest.clone();
    manifest_duplicate.extend_from_slice(&encoded_manifest[FILE_HEADER_LEN..]);
    assert!(decode_manifest("manifest.duplicate", &manifest_duplicate).is_err());
}
