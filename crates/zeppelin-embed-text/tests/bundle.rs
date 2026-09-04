#![allow(clippy::expect_used)]

use tempfile::tempdir;
use zeppelin_embed_text::bundle::{Bundle, BundleError};

mod common;

#[test]
fn a_bundle_with_a_corrupted_tensor_digest_is_refused_before_any_tensor_is_mapped() {
    let directory = tempdir().expect("tempdir");
    let path = directory.path().join("corrupt.zem");
    common::write_symmetric_fixture_bundle(&path);
    let mut bytes = std::fs::read(&path).expect("read fixture");
    bytes[4096] ^= 0x80;
    std::fs::write(&path, &bytes).expect("write bundle-digest corruption");
    assert!(matches!(
        Bundle::open(&path),
        Err(BundleError::BundleDigest)
    ));
    let trailer = bytes.len() - 16;
    let digest = xxhash_rust::xxh3::xxh3_128(&bytes[..trailer]);
    bytes[trailer..].copy_from_slice(&digest.to_le_bytes());
    std::fs::write(&path, bytes).expect("rewrite corrupted fixture");

    let error = match Bundle::open(&path) {
        Ok(_) => panic!("corrupted tensor must be rejected"),
        Err(error) => error,
    };
    assert!(matches!(error, BundleError::TensorDigest { .. }));
}

#[test]
fn a_two_tower_bundle_binds_document_and_query_by_name_and_a_one_tower_bundle_serves_both() {
    let directory = tempdir().expect("tempdir");
    let symmetric_path = directory.path().join("symmetric.zem");
    common::write_symmetric_fixture_bundle(&symmetric_path);
    let symmetric = Bundle::open(&symmetric_path).expect("open symmetric fixture");
    assert!(symmetric.is_symmetric());
    assert_eq!(symmetric.document_tower(), symmetric.query_tower());

    let pair_path = directory.path().join("pair.zem");
    common::write_fixture_bundle(
        &pair_path,
        &[
            common::FixtureTower::query("query-fixture"),
            common::FixtureTower::document("document-fixture"),
        ],
        b"aligned-pair-v1",
    );
    let pair = Bundle::open(&pair_path).expect("open paired fixture");
    assert!(!pair.is_symmetric());
    assert_eq!(pair.document_tower().embedding.model_id, "document-fixture");
    assert_eq!(pair.query_tower().embedding.model_id, "query-fixture");
    assert_eq!(pair.alignment_digest(), b"aligned-pair-v1");
}

#[test]
fn a_tower_weights_digest_must_equal_its_exact_tensor_section() {
    let directory = tempdir().expect("tempdir");
    let path = directory.path().join("wrong-weights-digest.zem");
    common::write_symmetric_fixture_bundle(&path);
    let mut bytes = std::fs::read(&path).expect("read fixture");
    let mut cursor = 25_usize;
    for _ in 0..2 {
        let length = bytes
            .get(cursor..cursor + 4)
            .and_then(|value| value.try_into().ok())
            .map(u32::from_le_bytes)
            .expect("tower string length") as usize;
        cursor += 4 + length;
    }
    let digest_length = bytes
        .get(cursor..cursor + 4)
        .and_then(|value| value.try_into().ok())
        .map(u32::from_le_bytes)
        .expect("weights digest length") as usize;
    assert_eq!(digest_length, 16);
    bytes[cursor + 4] ^= 0x80;
    let trailer = bytes.len() - 16;
    let digest = xxhash_rust::xxh3::xxh3_128(&bytes[..trailer]);
    bytes[trailer..].copy_from_slice(&digest.to_le_bytes());
    std::fs::write(&path, bytes).expect("rewrite fixture");

    let error = match Bundle::open(&path) {
        Ok(_) => panic!("forged tower weights digest must fail"),
        Err(error) => error,
    };
    assert!(matches!(error, BundleError::WeightsDigest { .. }));
}

#[test]
fn unknown_tokenizer_flag_bits_are_refused_instead_of_silently_dropped() {
    let directory = tempdir().expect("tempdir");
    let path = directory.path().join("unknown-tokenizer-flags.zem");
    common::write_symmetric_fixture_bundle(&path);
    let mut bytes = std::fs::read(&path).expect("read fixture");
    let tokenizer = tokenizer_offset(&bytes);
    bytes[tokenizer + 1] = 0x80;
    rewrite_digest(&mut bytes);
    std::fs::write(&path, bytes).expect("rewrite fixture");

    assert!(
        Bundle::open(path).is_err(),
        "unknown tokenizer flags must fail"
    );
}

#[test]
fn malformed_bundle_metadata_is_refused_at_the_exact_field_boundary() {
    let directory = tempdir().expect("tempdir");
    let base_path = directory.path().join("base.zem");
    common::write_symmetric_fixture_bundle(&base_path);
    let base = std::fs::read(&base_path).expect("read fixture");
    let offsets = fixture_offsets(&base);

    assert!(matches!(
        Bundle::open(directory.path().join("absent.zem")),
        Err(BundleError::Io { .. })
    ));
    assert_refused(directory.path(), "truncated", &base[..8], |_| {});
    assert_refused(directory.path(), "magic", &base, |bytes| bytes[0] ^= 1);
    assert_refused(directory.path(), "layout", &base, |bytes| {
        put_u32(bytes, 8, 2);
    });
    assert_refused(directory.path(), "root-reserved", &base, |bytes| {
        bytes[17] = 1;
    });
    assert_refused(directory.path(), "tower-count", &base, |bytes| {
        bytes[16] = 0;
    });
    assert_refused(directory.path(), "symmetric-role", &base, |bytes| {
        bytes[offsets.role] = 1;
    });
    assert_refused(directory.path(), "unknown-role", &base, |bytes| {
        bytes[offsets.role] = 9;
    });
    assert_refused(directory.path(), "normalization", &base, |bytes| {
        put_u16(bytes, offsets.normalization, 9);
    });
    assert_refused(directory.path(), "runtime", &base, |bytes| {
        put_u16(bytes, offsets.runtime, 9);
    });
    assert_refused(directory.path(), "compute-units", &base, |bytes| {
        put_u16(bytes, offsets.compute_units, 9);
    });
    assert_refused(directory.path(), "os-build", &base, |bytes| {
        bytes[offsets.os_build] = 9;
    });
    assert_refused(directory.path(), "pooling", &base, |bytes| {
        bytes[offsets.pooling] = 9;
    });
    assert_refused(directory.path(), "zero-dims", &base, |bytes| {
        put_u32(bytes, offsets.dims, 0);
    });
    assert_refused(directory.path(), "zero-max-tokens", &base, |bytes| {
        put_u32(bytes, offsets.max_tokens, 0);
    });
    assert_refused(directory.path(), "zero-hidden", &base, |bytes| {
        put_u32(bytes, offsets.hidden, 0);
    });
    assert_refused(directory.path(), "bad-head-width", &base, |bytes| {
        put_u32(bytes, offsets.hidden, 3);
        put_u16(bytes, offsets.heads, 2);
    });
    assert_refused(directory.path(), "tokenizer-kind", &base, |bytes| {
        bytes[offsets.tokenizer] = 9;
    });
    assert_refused(directory.path(), "tokenizer-reserved", &base, |bytes| {
        bytes[offsets.tokenizer + 2] = 1;
    });
    assert_refused(directory.path(), "bad-special-id", &base, |bytes| {
        put_u32(bytes, offsets.tokenizer + 4, 999);
    });
    assert_refused(directory.path(), "alpha", &base, |bytes| {
        bytes[offsets.alpha..offsets.alpha + 8].copy_from_slice(&2.0_f64.to_le_bytes());
    });
    assert_refused(directory.path(), "tensor-dtype", &base, |bytes| {
        bytes[offsets.tensor_dtype] = 9;
    });
    assert_refused(directory.path(), "tensor-rank", &base, |bytes| {
        bytes[offsets.tensor_rank] = 0;
    });
    assert_refused(directory.path(), "tensor-reserved", &base, |bytes| {
        bytes[offsets.tensor_reserved] = 1;
    });
    assert_refused(directory.path(), "tensor-shape", &base, |bytes| {
        put_u32(bytes, offsets.tensor_shape, 8);
    });
    assert_refused(directory.path(), "tensor-range", &base, |bytes| {
        bytes[offsets.tensor_offset..offsets.tensor_offset + 8]
            .copy_from_slice(&0_u64.to_le_bytes());
    });
    assert_refused(directory.path(), "header-length", &base, |bytes| {
        put_u32(bytes, 12, 1);
    });

    let bundle = Bundle::open(&base_path).expect("open fixture");
    assert_ne!(bundle.digest(), 0);
    assert_eq!(bundle.hybrid_alpha(), 0.5);
    let descriptor = zeppelin_embed_text::bundle::TensorDescriptor {
        name: "outside".to_owned(),
        dtype: zeppelin_embed_text::bundle::TensorDtype::F32,
        shape: vec![1],
        offset: usize::MAX,
        length: 4,
        digest: 0,
    };
    assert!(bundle.tensor_bytes(&descriptor).is_err());
}

#[derive(Clone, Copy)]
struct FixtureOffsets {
    role: usize,
    dims: usize,
    normalization: usize,
    max_tokens: usize,
    runtime: usize,
    compute_units: usize,
    os_build: usize,
    pooling: usize,
    hidden: usize,
    heads: usize,
    tokenizer: usize,
    alpha: usize,
    tensor_dtype: usize,
    tensor_rank: usize,
    tensor_reserved: usize,
    tensor_shape: usize,
    tensor_offset: usize,
}

fn fixture_offsets(bytes: &[u8]) -> FixtureOffsets {
    let mut cursor = 24_usize;
    let role = cursor;
    cursor += 1;
    skip_field(bytes, &mut cursor);
    skip_field(bytes, &mut cursor);
    skip_field(bytes, &mut cursor);
    let dims = cursor;
    cursor += 4;
    let normalization = cursor;
    cursor += 2;
    skip_field(bytes, &mut cursor);
    let max_tokens = cursor;
    cursor += 4;
    let runtime = cursor;
    cursor += 2;
    let compute_units = cursor;
    cursor += 2;
    let os_build = cursor;
    cursor += 1;
    let pooling = cursor;
    cursor += 1;
    cursor += 2 + 2;
    let hidden = cursor;
    cursor += 4;
    let heads = cursor;
    cursor += 2 + 4 + 4 + 4 + 4 + 4 + 4;
    let tokenizer = cursor;
    cursor += 4 + 4 + 4 + 4 + 4;
    let vocabulary = u32_at(bytes, cursor) as usize;
    cursor += 4;
    for _ in 0..vocabulary {
        skip_field(bytes, &mut cursor);
        cursor += 4;
    }
    let alpha = cursor;
    cursor += 8 + 4;
    skip_field(bytes, &mut cursor);
    let tensor_dtype = cursor;
    let tensor_rank = cursor + 1;
    let tensor_reserved = cursor + 2;
    let tensor_shape = cursor + 4;
    let rank = usize::from(bytes[tensor_rank]);
    let tensor_offset = tensor_shape + rank * 4;
    FixtureOffsets {
        role,
        dims,
        normalization,
        max_tokens,
        runtime,
        compute_units,
        os_build,
        pooling,
        hidden,
        heads,
        tokenizer,
        alpha,
        tensor_dtype,
        tensor_rank,
        tensor_reserved,
        tensor_shape,
        tensor_offset,
    }
}

fn skip_field(bytes: &[u8], cursor: &mut usize) {
    *cursor += 4 + u32_at(bytes, *cursor) as usize;
}

fn assert_refused(
    directory: &std::path::Path,
    name: &str,
    base: &[u8],
    mutate: impl FnOnce(&mut [u8]),
) {
    let mut bytes = base.to_vec();
    mutate(&mut bytes);
    if bytes.len() >= 24 {
        rewrite_digest(&mut bytes);
    }
    let path = directory.join(format!("{name}.zem"));
    std::fs::write(&path, bytes).expect("write malformed fixture");
    assert!(Bundle::open(path).is_err(), "{name} must be refused");
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn tokenizer_offset(bytes: &[u8]) -> usize {
    let mut cursor = 25_usize;
    for _ in 0..3 {
        let length = u32_at(bytes, cursor) as usize;
        cursor += 4 + length;
    }
    cursor += 4 + 2;
    let prefix = u32_at(bytes, cursor) as usize;
    cursor += 4 + prefix;
    cursor + 4 + 2 + 2 + 1 + 1 + 2 + 2 + 4 + 2 + 4 + 4 + 4 + 4 + 4 + 4
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("u32 field"))
}

fn rewrite_digest(bytes: &mut [u8]) {
    let trailer = bytes.len() - 16;
    let digest = xxhash_rust::xxh3::xxh3_128(&bytes[..trailer]);
    bytes[trailer..].copy_from_slice(&digest.to_le_bytes());
}
