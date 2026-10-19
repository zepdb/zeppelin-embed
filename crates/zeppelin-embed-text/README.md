# Zeppelin Embed Text

Local text embedding and retrieval over Zeppelin Embed stores. Load a `.zem`
model bundle, ingest text with `TextStore::ingest_text`, and search with
`TextStore::query_text` using dense, lexical, or hybrid retrieval.

Requires macOS on Apple silicon and Xcode with the Metal toolchain. MLX runs
the embedding models; the crate also includes a CoreML query runtime.

```sh
xcodebuild -downloadComponent MetalToolchain
cargo add zeppelin-embed-text
```

Applications supply a local `.zem` model bundle to `TextStore::open`.

See the [project repository](https://github.com/zepdb/zeppelin-embed) for model
tooling and API usage.

Licensed under [GPL-3.0-only](https://www.gnu.org/licenses/gpl-3.0.html).

## CI and the local text release gate

`.github/workflows/text-ci.yml` builds and lints text and FFI with `text`
enabled on hosted `macos-14`, runs every non-ignored test in both crates,
and builds the full graph+text XCFramework to run all Swift `TextStoreTests`.
Rust releases require these checks before publishing either crate.

The dedicated real-model job runs
`default_model::real_default_model_downloads_verifies_and_reopens_offline` and
`release_tokenizer::release_tokenizer_ids_match_hf_0_19_1_for_both_towers`.
It downloads through `resolve_model` from HF, caches `ZE_MODEL_CACHE` keyed on
`DEFAULT_MODEL_REVISION`, and verifies cache hits. Locally the resolver uses
`ZE_MODEL_CACHE` or `~/Library/Caches/zeppelin-embed/models`.

Only the following ignored tests require the local release gate. Run from the
repository root outside the sandbox with the named fixtures and native
MLX/CoreML available. Historical LEAF/Arctic/FP32 goldens require their original
models; substituting the published FP16 default would invalidate the assertions.

| Ignored tests | Required inputs | Local command |
| --- | --- | --- |
| Default-model inference | Verified HF cache or network | `cargo test --locked -p zeppelin-embed-text --test default_model real_default_model_downloads_and_answers_queries -- --ignored --exact` |
| LEAF/Arctic parity and placement | `ZE_TEXT_TEST_BUNDLE_DIR`: original LEAF v1.5 pair and Arctic v2 symmetric bundles; LEAF `.mlmodelc` beside them or `ZE_COREML_QUERY_MODEL` | `cargo test --locked -p zeppelin-embed-text --test arch --test placement -- --ignored` |
| clean71 FP32 parity | `ZE_CLEAN71_PACK`, ze-model Python dependencies | `cargo test --locked -p zeppelin-embed-text --test arctic_query -- --ignored` |
| clean71 CoreML plan and parity | `ZE_CLEAN71_COREML_MODEL`, `ZE497_PACK`, `ZE497_COREML`, `ZE497_PYTHON` | `cargo test --locked -p zeppelin-embed-text --test clean71_coreml -- --ignored` |
| Release smoke and FP16 document parity | HF cache or `ZE498_RELEASE`; `ZE498_PACK`, `ZE498_PYTHON`; `ZE_QUERY_COREML` unset | `cargo test --locked -p zeppelin-embed-text --test release -- --ignored` |
| 30,001-document maintenance | Unsandboxed MLX runtime | `cargo test --locked -p zeppelin-embed-text --test text_api -- --ignored` |

`model_cache::measure_large_cache_open` is an ignored manual benchmark, not a
correctness gate: `cargo test --locked -p zeppelin-embed-text --test model_cache measure_large_cache_open -- --ignored --exact`.
