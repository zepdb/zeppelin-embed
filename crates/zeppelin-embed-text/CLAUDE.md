# Text embedding crate guide

- Step 08 admits pinned pure-Rust `unicode-normalization-alignments` 0.1.12
  and `unicode_categories` 0.1.1 in this outer crate only. They are the source
  tokenizers 0.22.2 Unicode helpers and avoid hand-maintained normalization
  approximations. Their MIT/Apache licenses require no deny-policy exception;
  the dependency audit and linked-size comparison belong to Step 08 evidence.
  No dependency enters the core. WordPiece follows the source BERT order:
  clean controls/whitespace, separate CJK, NFD/strip Mn, scalar lowercase,
  Unicode punctuation splitting, then WordPiece with a 100-character limit.

- MLX is bound through `mlx-rs` 0.25.3 with its `metal` feature so only this
  outer crate owns the CMake, C++, Metal, `cmake`, and `bindgen` build graph.
- `cmake` builds pinned `mlx-c`; `bindgen` generates that C ABI, and both are
  build-only transitive dependencies admitted solely under `mlx-sys`.
- `libloading`'s ISC license and the unmaintained `paste` macro advisory are
  recorded deny exceptions forced by the pinned `mlx-rs` graph; neither adds
  runtime model, network, tokenizer, or serialization behavior.
- `.zem` layout 3 stores all tower records followed by one tokenizer record
  per tower in the same order; a symmetric bundle stores one of each. Each
  tokenizer carries f64 scores and an explicit length-prefixed charsmap (empty
  when absent). Layouts 1 and 2 are refused with a typed unsupported-version
  error; re-bake pre-launch bundles.
- Runtime model inputs remain the explicit `.zem` layout. ZE-501 model
  download now lives here: NSURLSession in `objc/ze_model.m`, a pinned
  `zeppelin-model.json` manifest, CommonCrypto streaming SHA-256, and an
  atomically installed verified cache. No new crate or deny-policy exception.
  JSON beyond this manifest, safetensors, and training remain in `tools/ze-model`.
  Model refs require a repo, immutable commit SHA, and manifest SHA-256;
  the built-in default is `aghatage/zeppelin-embed-clean71-arctic-m-v2` at
  `d64b817c4021a3f1b9bded3152d9afdf9ac813e7`, with manifest SHA-256
  `123c6308e00f769c68498f9df60dfb9efc688f58c4ff2c4da266f978fcb215dc`.
  Cache option wins over `ZE_MODEL_CACHE`, then defaults to
  `~/Library/Caches/zeppelin-embed/models/<repo with %2F separator>/<revision>`.
  Offline option or `ZE_MODEL_OFFLINE=1` forbids network; endpoint option wins
  over `ZE_MODEL_ENDPOINT`. Plain HTTP requires `allow_http` or
  `ZE_MODEL_ALLOW_HTTP=1`, for test fixtures only. HF_TOKEN is sent only over
  HTTPS to the exact `huggingface.co` host by default; `send_token_to_endpoint`
  separately authorizes the endpoint's exact host over HTTPS. HTTP never gets
  the token. The same authorization predicate governs initial requests and
  every redirect; other hosts get a request with Authorization removed (HF CDN
  signed URLs need no token). C request flags reuse the reserved slot without
  changing size or offsets. Existing explicit-path open is retained.
- The MLX runtime owns one named embed thread and one non-default stream. Calls
  reach it only through bounded standard-library channels, and a process-wide
  mutex serializes MLX creation, evaluation, and destruction because the
  pinned Rust binding documents task-local defaults but no cross-thread C ABI
  safety guarantee; the parallel public-path test otherwise crashes in MLX.
- A single MLX evaluation covers at most 32 rows and targets 4,096 padded
  tokens, measured by ZE-525's native release-FP16 probes. A row longer than
  that budget runs alone, trimmed to its own length; size alone is not refused.
  Each contiguous split trims trailing padding and completes before the next
  split is built. The release document tower supports 8,192 model tokens;
  ingest's default chunk size counts approximate source words, not model tokens.
  Full-ingest watchdog/throughput qualification is tracked in ZE-525 evidence.
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

- Step 08 additionally appends `;ze-text-wordpiece=2` for the corrected BERT
  tokenizer interpretation, in both public and document epochs. ZE-514 appends
  `;ze-text-unigram=2` for corrected Unigram evaluation. Legacy uncased
  WordPiece implies BERT default accent stripping;
  new exports record the effective strip flag and reject unsupported contracts.
