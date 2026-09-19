#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use std::sync::Arc;
use tempfile::tempdir;
use zeppelin_embed::epoch::ComputeUnits;
use zeppelin_embed_text::bundle::Bundle;
use zeppelin_embed_text::runtime::mlx::MlxRuntime;
use zeppelin_embed_text::runtime::{ModelRuntime, RuntimeError};
use zeppelin_embed_text::tower::{TokenBatch, TowerRole};

#[path = "fixtures/ze115/reference.rs"]
#[allow(clippy::excessive_precision)]
mod reference;

fn fixture(architecture: &str, pooling: u8, dims: u32) -> Arc<Bundle> {
    let mut bytes = match architecture {
        "bert" => include_bytes!("fixtures/ze115/bert.zem").to_vec(),
        "gte" => include_bytes!("fixtures/ze115/gte.zem").to_vec(),
        _ => panic!("unknown fixture"),
    };
    bytes[reference::POOLING_OFFSET] = pooling;
    bytes[reference::DIMS_OFFSET..reference::DIMS_OFFSET + 4].copy_from_slice(&dims.to_le_bytes());
    let end = bytes.len() - 16;
    let digest = xxhash_rust::xxh3::xxh3_128(&bytes[..end]);
    bytes[end..].copy_from_slice(&digest.to_le_bytes());
    let directory = tempdir().expect("fixture directory");
    let path = directory.path().join("tiny.zem");
    std::fs::write(&path, bytes).expect("write invented model");
    Arc::new(Bundle::open(path).expect("valid tiny transformer"))
}

fn tokens(rows: usize) -> TokenBatch {
    let ids = [[2, 5, 6, 3, 0], [2, 6, 3, 0, 0]];
    let masks = [[1., 1., 1., 1., 0.], [1., 1., 1., 0., 0.]];
    TokenBatch::new(
        ids.iter().cycle().take(rows).flatten().copied().collect(),
        masks.iter().cycle().take(rows).flatten().copied().collect(),
        rows,
        5,
    )
    .expect("rectangular rows with distinct masks")
}

#[test]
fn tiny_transformers_match_independent_cpu_reference_across_pooling_and_chunk_boundaries() {
    assert_eq!(MlxRuntime::max_eval_rows(), 32);
    for (architecture, expected) in [
        ("bert", reference::BERT_REFERENCE),
        ("gte", reference::GTE_REFERENCE),
    ] {
        for (pool_index, pooling) in [1, 2, 3].into_iter().enumerate() {
            for dims in [1, 3] {
                let bundle = fixture(architecture, pooling, dims);
                for compute in [ComputeUnits::Cpu, ComputeUnits::CpuAndGpu] {
                    let mut runtime = MlxRuntime::load_for_compute(
                        Arc::clone(&bundle),
                        TowerRole::Query,
                        compute,
                    )
                    .expect("real tiny transformer runtime");
                    assert_eq!(runtime.identity().name, "mlx-c");
                    assert_eq!(runtime.identity().gpu, compute == ComputeUnits::CpuAndGpu);
                    runtime.warm().expect("one-token warmup");
                    for rows in [2, 33] {
                        let output = runtime
                            .embed_batch(&tokens(rows))
                            .expect("real MLX evaluation");
                        assert_eq!((output.rows(), output.dims()), (rows, dims as usize));
                        for (row, actual) in output.values().chunks_exact(dims as usize).enumerate()
                        {
                            for (column, actual) in actual.iter().enumerate() {
                                let oracle = expected[pool_index][row % 2][column];
                                assert!(
                                    (actual - oracle).abs() < 2e-5,
                                    "{architecture} pool={pooling} dims={dims} compute={compute:?} rows={rows} row={row} col={column}: {actual} vs independent {oracle}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn mlx_rejects_unsupported_placement_and_malformed_chunk_shapes() {
    let bundle = fixture("bert", 1, 3);
    for compute in [ComputeUnits::CpuAndNeuralEngine, ComputeUnits::All] {
        assert!(matches!(
            MlxRuntime::load_for_compute(Arc::clone(&bundle), TowerRole::Document, compute),
            Err(RuntimeError::Mlx(detail)) if detail == "MLX does not target the Neural Engine"
        ));
    }
    let mut runtime = MlxRuntime::load_for_compute(bundle, TowerRole::Document, ComputeUnits::Cpu)
        .expect("CPU runtime");
    let mut invalid = tokens(33);
    invalid.token_ids.pop();
    assert!(
        matches!(runtime.embed_batch(&invalid), Err(RuntimeError::Shape(detail)) if detail == "token batch chunk is truncated")
    );
    let mut invalid = tokens(33);
    invalid.attention_mask.pop();
    assert!(
        matches!(runtime.embed_batch(&invalid), Err(RuntimeError::Shape(detail)) if detail == "attention chunk is truncated")
    );
    let invalid = TokenBatch {
        token_ids: vec![],
        attention_mask: vec![],
        rows: usize::MAX,
        tokens_per_row: 5,
    };
    assert!(
        matches!(runtime.embed_batch(&invalid), Err(RuntimeError::Shape(detail)) if detail == "embedding output size overflow")
    );
    assert!(
        runtime.embed_batch(&tokens(2)).is_ok(),
        "errors do not poison the runtime"
    );
    let mut oversized =
        MlxRuntime::load_for_compute(fixture("gte", 1, 4), TowerRole::Document, ComputeUnits::Cpu)
            .expect("load oversized declaration");
    assert!(
        matches!(oversized.embed_batch(&tokens(2)), Err(RuntimeError::Shape(detail)) if detail == "declared output dimensions exceed architecture output")
    );
}

#[test]
fn token_batches_preserve_every_row_when_padding_and_refuse_shape_loss() {
    let original =
        TokenBatch::new(vec![7, 8, 9, 10], vec![1., 0., 1., 1.], 2, 2).expect("valid input");
    assert_eq!(original.padded_to(2).expect("same width"), original);
    assert_eq!(
        original.padded_to(4).expect("pad both rows"),
        TokenBatch::new(
            vec![7, 8, 0, 0, 9, 10, 0, 0],
            vec![1., 0., 0., 0., 1., 1., 0., 0.],
            2,
            4,
        )
        .expect("expected padded batch")
    );
    assert!(
        matches!(original.padded_to(1), Err(RuntimeError::Shape(detail)) if detail == "cannot pad 2 tokens per row down to 1")
    );
    assert_eq!(
        TokenBatch::new(vec![], vec![], usize::MAX, 2),
        Err("token batch shape overflow")
    );
    assert_eq!(
        TokenBatch::new(vec![], vec![], 0, 2),
        Err("token ids do not match the declared batch shape")
    );
    assert_eq!(
        TokenBatch::new(vec![1], vec![], 1, 1),
        Err("attention mask does not match the declared batch shape")
    );
    let mut invalid = original.clone();
    invalid.token_ids.pop();
    assert!(
        matches!(invalid.padded_to(3), Err(RuntimeError::Shape(detail)) if detail == "token id row is out of range")
    );
    let mut invalid = original;
    invalid.attention_mask.pop();
    assert!(
        matches!(invalid.padded_to(3), Err(RuntimeError::Shape(detail)) if detail == "attention row is out of range")
    );
}

#[test]
fn mapped_model_tensor_metadata_is_rejected_with_qualified_error_context() {
    let root = tempdir().expect("root");
    let path = root.path().join("mutated.zem");
    let original = include_bytes!("fixtures/ze115/bert.zem");
    let name = b"document/embeddings.word_embeddings.weight";
    let offset = original
        .windows(name.len())
        .position(|s| s == name)
        .expect("first descriptor");
    let dtype = offset + name.len();
    for (code, width) in [(1_u8, 8_u32), (2, 8), (3, 16), (4, 4)] {
        let mut bytes = original.to_vec();
        bytes[dtype] = code;
        // Preserve the byte count while changing the scalar representation.
        bytes[dtype + 8..dtype + 12].copy_from_slice(&width.to_le_bytes());
        rewrite_fixture(&path, bytes);
        let bundle = Arc::new(Bundle::open(&path).expect("valid non-F32 descriptor"));
        assert!(
            matches!(MlxRuntime::load_for_compute(bundle, TowerRole::Document, ComputeUnits::Cpu),
            Err(RuntimeError::UnsupportedDtype(name)) if name == "document/embeddings.word_embeddings.weight")
        );
    }
    let mut bytes = original.to_vec();
    bytes[offset + name.len() - 1] = b'X';
    rewrite_fixture(&path, bytes);
    let bundle = Arc::new(Bundle::open(&path).expect("valid descriptor with missing model tensor"));
    assert!(matches!(MlxRuntime::load(bundle, TowerRole::Document),
        Err(RuntimeError::MissingTensor(name)) if name == "document/embeddings.word_embeddings.weight"));
    rewrite_fixture(&path, original.to_vec());
    let clean = Arc::new(Bundle::open(&path).expect("restored valid model"));
    assert!(MlxRuntime::load(clean, TowerRole::Document).is_ok());
}

fn rewrite_fixture(path: &std::path::Path, mut bytes: Vec<u8>) {
    let end = bytes.len() - 16;
    let digest = xxhash_rust::xxh3::xxh3_128(&bytes[..end]);
    bytes[end..].copy_from_slice(&digest.to_le_bytes());
    std::fs::write(path, bytes).expect("rewrite tiny fixture with correct whole-file digest");
}
