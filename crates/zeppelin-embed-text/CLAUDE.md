# Text embedding crate guide

- MLX is bound through `mlx-rs` 0.25.3 with its `metal` feature so only this
  outer crate owns the CMake, C++, Metal, `cmake`, and `bindgen` build graph.
- `cmake` builds pinned `mlx-c`; `bindgen` generates that C ABI, and both are
  build-only transitive dependencies admitted solely under `mlx-sys`.
- `libloading`'s ISC license and the unmaintained `paste` macro advisory are
  recorded deny exceptions forced by the pinned `mlx-rs` graph; neither adds
  runtime model, network, tokenizer, or serialization behavior.
- Runtime model inputs are the explicit `.zem` layout only. JSON,
  safetensors, network access, and training remain in `tools/ze-model`.
- The MLX runtime owns one named embed thread and one non-default stream. Calls
  reach it only through bounded standard-library channels, and a process-wide
  mutex serializes MLX creation, evaluation, and destruction because the
  pinned Rust binding documents task-local defaults but no cross-thread C ABI
  safety guarantee; the parallel public-path test otherwise crashes in MLX.
- A single MLX evaluation covers at most 32 rows and completes before the next
  chunk is built. This bounds Metal command-buffer lifetime below the macOS
  watchdog on real, long-document batches.
- MLX CPU/GPU and CoreML placement remain runtime epoch fields. CoreML exports
  fixed-shape query towers through `tools/ze-model`; accepting backend drift
  within tolerance remains an open owner decision.
- Produced vectors are dimension-checked, truncated before normalization,
  materialized as f32, and checked for unit norm before core ingest.
- Before the MLX host copy, flatten pooled/truncated output on the owning
  stream: CLS and MRL can leave strided rows that `try_as_slice` does not gather.
  If scalar truncation leaves the flattened vector strided, gather its logical
  elements before reading consecutive host memory.
  `TextStore` appends `;ze-text-output-layout=2` to the source model version
  when deriving both public and persisted epochs. This versions the corrected
  evaluation behavior without changing bundle bytes or format layouts. Older
  text stores fail with `EpochMismatch` and require re-embedding into a fresh
  store; never relabel their existing vectors as corrected.
