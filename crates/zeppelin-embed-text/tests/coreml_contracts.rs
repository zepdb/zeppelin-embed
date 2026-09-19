#![cfg(target_os = "macos")]
#![allow(clippy::expect_used, clippy::indexing_slicing)]

use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::{TempDir, tempdir};
use zeppelin_embed_text::runtime::coreml::{ComputeUnits, CoreMlRuntime};
use zeppelin_embed_text::runtime::{ModelRuntime, RuntimeError};
use zeppelin_embed_text::tower::TokenBatch;

mod common;

fn compile_fixture() -> (TempDir, PathBuf) {
    let root = tempdir().expect("CoreML compiler directory");
    let source = root.path().join("tiny.mlmodel");
    std::fs::write(&source, include_bytes!("fixtures/ze115/tiny.mlmodel"))
        .expect("write portable model");
    let output = Command::new("xcrun")
        .args(["coremlcompiler", "compile"])
        .arg(&source)
        .arg(root.path())
        .output()
        .expect("run platform CoreML compiler");
    assert!(
        output.status.success(),
        "CoreML compilation failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let model = root.path().join("tiny.mlmodelc");
    assert!(model.is_dir());
    (root, model)
}

#[test]
fn coreml_runtime_evaluates_exact_rows_and_masks_under_every_requested_policy() {
    let (_root, model) = compile_fixture();
    let tokens = TokenBatch::new(
        vec![7, 11, 3, 9, -2, 5],
        vec![1., 0., 0.01, -1., f32::NAN, f32::INFINITY],
        3,
        2,
    )
    .expect("three mask cases");
    for (policy, name, gpu) in [
        (ComputeUnits::Cpu, "coreml-cpu", false),
        (ComputeUnits::CpuAndGpu, "coreml-cpu-gpu", true),
        (ComputeUnits::CpuAndNeuralEngine, "coreml-cpu-ane", false),
        (ComputeUnits::All, "coreml-all", true),
    ] {
        let mut runtime = CoreMlRuntime::load(&model, 2, 2, policy).expect("actual compiled model");
        assert_eq!(runtime.sequence(), 2);
        assert_eq!(runtime.identity().name, name);
        assert_eq!(runtime.identity().gpu, gpu);
        runtime.warm().expect("real one-row warmup");
        let output = runtime
            .embed_batch(&tokens)
            .expect("actual CoreML prediction");
        assert_eq!((output.rows(), output.dims()), (3, 2));
        assert_eq!(output.values(), [8., 11., 4., 9., -2., 6.]);
        assert_eq!(
            runtime
                .embed_batch(&tokens)
                .expect("repeat after warm and prediction")
                .values(),
            output.values()
        );
        // Dropping the runtime exercises exclusive ownership of the native model.
    }
}

#[test]
fn coreml_reports_bad_artifacts_paths_shapes_and_prediction_width_without_partial_results() {
    let (root, model) = compile_fixture();
    for (sequence, dims) in [(0, 2), (2, 0)] {
        assert!(
            matches!(CoreMlRuntime::load(&model, sequence, dims, ComputeUnits::Cpu), Err(RuntimeError::Shape(detail)) if detail == "CoreML sequence and dims must be non-zero")
        );
    }
    let invalid_utf8 = PathBuf::from(std::ffi::OsString::from_vec(vec![0xff]));
    assert!(
        matches!(CoreMlRuntime::load(&invalid_utf8, 2, 2, ComputeUnits::Cpu), Err(RuntimeError::Mlx(detail)) if detail == "CoreML model path is not valid UTF-8")
    );
    assert!(
        matches!(CoreMlRuntime::load(Path::new("invalid\0path"), 2, 2, ComputeUnits::Cpu), Err(RuntimeError::Mlx(detail)) if detail == "CoreML model path contains NUL")
    );
    assert!(
        matches!(CoreMlRuntime::load(&root.path().join("absent.mlmodelc"), 2, 2, ComputeUnits::Cpu), Err(RuntimeError::Mlx(detail)) if !detail.is_empty())
    );
    let mut runtime = CoreMlRuntime::load(&model, 2, 2, ComputeUnits::Cpu).expect("clean runtime");
    let wrong_width = TokenBatch::new(vec![1, 2, 3], vec![1.; 3], 1, 3).expect("different width");
    assert!(
        matches!(runtime.embed_batch(&wrong_width), Err(RuntimeError::Shape(detail)) if detail == "CoreML model expects 2 tokens per row, batch carries 3")
    );
    let overflow = TokenBatch {
        token_ids: vec![],
        attention_mask: vec![],
        rows: usize::MAX,
        tokens_per_row: 2,
    };
    assert!(
        matches!(runtime.embed_batch(&overflow), Err(RuntimeError::Shape(detail)) if detail == "CoreML batch size overflows")
    );
    let mut invalid = TokenBatch::new(vec![7, 11, 3, 9], vec![1.; 4], 2, 2).expect("two rows");
    invalid.token_ids.pop();
    assert!(
        matches!(runtime.embed_batch(&invalid), Err(RuntimeError::Shape(detail)) if detail == "token id row is out of range")
    );
    invalid.token_ids.push(9);
    invalid.attention_mask.pop();
    assert!(
        matches!(runtime.embed_batch(&invalid), Err(RuntimeError::Shape(detail)) if detail == "attention mask row is out of range")
    );
    let valid = TokenBatch::new(vec![7, 11], vec![1., 0.], 1, 2).expect("valid row");
    assert_eq!(
        runtime
            .embed_batch(&valid)
            .expect("errors leave model usable")
            .values(),
        [8., 11.]
    );
    let mut wrong_dims = CoreMlRuntime::load(&model, 2, 3, ComputeUnits::Cpu)
        .expect("declaration checked at prediction");
    assert!(
        matches!(wrong_dims.embed_batch(&valid), Err(RuntimeError::Mlx(detail)) if detail == "output embedding has an unexpected width")
    );
    let mut wrong_sequence =
        CoreMlRuntime::load(&model, 3, 2, ComputeUnits::Cpu).expect("declaration checked by model");
    assert!(
        matches!(wrong_sequence.embed_batch(&wrong_width), Err(RuntimeError::Mlx(detail)) if !detail.is_empty())
    );
}

#[test]
fn discovered_coreml_query_model_is_reported_and_used_by_the_store() {
    let (_root, model) = compile_fixture();
    let output = Command::new(std::env::current_exe().expect("test executable"))
        .args(["--exact", "coreml_store_child", "--nocapture"])
        .env("ZE115_COREML_FIXTURE", &model)
        .env("ZE_QUERY_COREML", &model)
        .env("ZE_QUERY_COREML_TOKENS", "2")
        .output()
        .expect("isolated environment for model discovery");
    assert!(
        output.status.success(),
        "CoreML store child failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ZE115 real CoreML store query completed")
    );
}

#[test]
fn coreml_store_child() {
    if std::env::var_os("ZE115_COREML_FIXTURE").is_none() {
        return;
    }
    use zeppelin_embed_text::{IngestOptions, Legs, QueryOptions, TextDocument, TextStore};
    let root = tempdir().expect("store root");
    let model = root.path().join("document.zem");
    common::write_symmetric_fixture_bundle(&model);
    let store = TextStore::open(root.path().join("store"), &model, Default::default())
        .expect("actual mixed runtime store");
    let backend = store.query_backend();
    assert_eq!(backend.runtime.name, "coreml-cpu-ane");
    assert_eq!(
        backend.requested_compute_units,
        zeppelin_embed::epoch::ComputeUnits::CpuAndNeuralEngine
    );
    assert_eq!(
        backend.observed_compute_units, None,
        "requested policy is not observed placement"
    );
    assert_eq!(backend.sequence_length, Some(2));
    let report = store
        .ingest_text(
            &[TextDocument::new(7, 1, "bronze")],
            IngestOptions::default(),
        )
        .expect("MLX document ingest");
    assert_eq!(report.chunks, 1);
    assert!(report.all_threads_joined);
    // An empty query contains exactly CLS+SEP, the tiny model's fixed width.
    let result = store
        .query_text_with_diagnostics("", QueryOptions::new(1).with_legs(Legs::Dense))
        .expect("CoreML query");
    assert_eq!(result.hits.len(), 1);
    assert_eq!(result.hits[0].doc_id, 7);
    assert_eq!(result.query_tokens, 2);
    assert_eq!(result.embedding_calls, 1);
    assert_eq!(result.backend, Some(backend));
    store.close().expect("join MLX and CoreML owner");
    println!("ZE115 real CoreML store query completed");
}
