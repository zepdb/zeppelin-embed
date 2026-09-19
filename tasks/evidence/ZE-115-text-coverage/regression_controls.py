#!/usr/bin/env python3
"""Isolated mutants: typed failures must fire; always restore exact source bytes."""
import hashlib
import os
from pathlib import Path
import subprocess

ROOT = Path.cwd()
OUT = Path('/tmp/ze-115-evidence')
PREFIX = Path('crates/zeppelin-embed-text/src')
controls = [
    ('attention-mask', 'arch/bert.rs', 'Array::from_f32(1.0e9)', 'Array::from_f32(0.0)', 'runtime_contracts', 'tiny_transformers_match_independent_cpu_reference_across_pooling_and_chunk_boundaries'),
    ('query-routing', 'ingest.rs', 'TowerRole::Query => query.embed_controlled(tokens, &mut checkpoint),', 'TowerRole::Query => document.embed_batch_controlled(tokens, &mut checkpoint),', 'lifecycle_contracts', 'paired_towers_keep_query_routing_and_chunk_deletion_across_reopen'),
    ('unigram-scores', 'tokenizer.rs', 'let candidate = start_score + score;', 'let candidate = start_score - score;', 'tokenizer_contracts', 'bundle_unigram_uses_global_scores_normalization_and_rectangular_masks'),
    ('coreml-mask', 'runtime/coreml.rs', 'if *value > 0.0 { 1 } else { 0 }', 'if *value >= 0.0 { 1 } else { 0 }', 'coreml_contracts', 'coreml_runtime_evaluates_exact_rows_and_masks_under_every_requested_policy'),
    ('mlx-placement', 'runtime/mlx.rs', 'return Err(RuntimeError::Mlx(\n                    "MLX does not target the Neural Engine".to_owned(),\n                ));', '(Device::cpu(), false)', 'runtime_contracts', 'mlx_rejects_unsupported_placement_and_malformed_chunk_shapes'),
    ('tensor-dtype', 'runtime/mlx.rs', 'return Err(RuntimeError::UnsupportedDtype(qualified));', 'return Err(RuntimeError::MissingTensor(qualified));', 'runtime_contracts', 'mapped_model_tensor_metadata_is_rejected_with_qualified_error_context'),
    ('padding', 'tower.rs', '                0,\n            );', '                1,\n            );', 'runtime_contracts', 'token_batches_preserve_every_row_when_padding_and_refuse_shape_loss'),
    ('broken-discovery', 'ingest.rs', 'model.is_dir().then_some((model, tokens))', 'None', 'lifecycle_contracts', 'discovered_broken_coreml_artifact_fails_open_without_silent_backend_fallback'),
    ('query-padding', 'bundle.rs', 'self.tokenize_query(text)?.padded_to(width)', 'self.tokenize_query(text)', 'tokenizer_contracts', 'bundle_query_padding_preserves_special_tokens_and_refuses_truncation'),
    ('coreml-zero-shape', 'runtime/coreml.rs', 'if sequence == 0 || dims == 0 {', 'if false {', 'coreml_contracts', 'coreml_reports_bad_artifacts_paths_shapes_and_prediction_width_without_partial_results'),
    ('coreml-discovery', 'ingest.rs', 'model.is_dir().then_some((model, tokens))', 'None', 'coreml_contracts', 'discovered_coreml_query_model_is_reported_and_used_by_the_store'),
]
env = dict(os.environ, CARGO_TARGET_DIR='target/ze115-coverage')
for label, filename, old, new, target, test in controls:
    path = ROOT / PREFIX / filename
    original = path.read_bytes()
    source = original.decode()
    assert source.count(old) == 1, (label, source.count(old))
    sha = hashlib.sha256(original).hexdigest()
    command = ['cargo', 'test', '-p', 'zeppelin-embed-text', '--test', target, test, '--', '--exact', '--nocapture']
    try:
        path.write_text(source.replace(old, new))
        red = subprocess.run(command, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        (OUT / ('control-' + label + '-red.log')).write_bytes(red.stdout)
        output = red.stdout.decode(errors='replace')
        assert red.returncode == 101 and f'test {test} ... FAILED' in output and 'test result: FAILED' in output, (label, red.returncode, output)
    finally:
        path.write_bytes(original)
    assert hashlib.sha256(path.read_bytes()).hexdigest() == sha
    green = subprocess.run(command, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    (OUT / ('control-' + label + '-green.log')).write_bytes(green.stdout)
    assert green.returncode == 0 and f'test {test} ... ok' in green.stdout.decode(errors='replace'), label
    print(f'{label}: RED101 intended test failure; restored SHA256={sha}; GREEN0 {test}', flush=True)
print(f'{len(controls)} controls caught and restored')
