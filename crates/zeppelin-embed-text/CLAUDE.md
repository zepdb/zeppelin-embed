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
- Produced vectors are dimension-checked, truncated before normalization,
  materialized as f32, and checked for unit norm before core ingest.
