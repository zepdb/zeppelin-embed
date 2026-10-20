# Zeppelin Embed Text

Local text embedding and dense (vector), lexical (BM25), or hybrid retrieval
without supplying your own vectors. Requires macOS on Apple silicon; source
builds need Xcode with the Metal toolchain.

```sh
xcodebuild -downloadComponent MetalToolchain
cargo add zeppelin-embed-text
```

Open the published default model, ingest, query every leg, and delete:

```rust
use zeppelin_embed_text::{
    IngestOptions, Legs, ModelOpenOptions, ModelRef, QueryOptions, TextDocument, TextStore,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let store = TextStore::open_model(
        "text-store", &ModelRef::default_model(), ModelOpenOptions::default(),
    )?;
    store.ingest_text(
        &[TextDocument::new(1, 1, "The bronze zeppelin flies above the city.")],
        IngestOptions::default(),
    )?;
    for legs in [Legs::Dense, Legs::Lexical, Legs::Hybrid] {
        let hits = store.query_text("bronze zeppelin", QueryOptions::new(5).with_legs(legs))?;
        println!("{hits:?}");
    }
    store.delete_text(&[1])?; // deletes every chunk of this caller document
    store.close()?;
    Ok(())
}
```

`ModelRef::default_model()` uses the [published model card](https://huggingface.co/aghatage/zeppelin-embed-clean71-arctic-m-v2/blob/d64b817c4021a3f1b9bded3152d9afdf9ac813e7/README.md)
at revision `d64b817c4021a3f1b9bded3152d9afdf9ac813e7`.
The first open downloads and verifies the pinned model; subsequent opens reuse
the verified cache. Set `ModelOpenOptions.cache_dir` to override `ZE_MODEL_CACHE`
and the default `~/Library/Caches/zeppelin-embed/models`. Set `offline: true`
or `ZE_MODEL_OFFLINE=1` to prohibit networking; a cache miss returns
`TextError::ModelResolution(ModelError::OfflineMiss)`. `ZE_MODEL_ENDPOINT` or
`ModelOpenOptions.endpoint` overrides the download endpoint.

`ModelOpenOptions.progress` receives synchronous byte counts on the calling
thread. Intermediate reports require both at least 64 KiB and at least 100 ms
since the last report; each verified file also reports completion. Initial
`(0, 0)` means fetching the manifest. Do not reenter model resolution from the
callback.

For a preinstalled bundle, `TextStore::open(path, bundle_path, options)` remains
available. Caller document IDs must fit in 96 bits; a higher revision replaces
the previous document and its chunks. See the [root text API guide](../../README.md#text-api-macos-apple-silicon)
for tower details, download size, and measured performance.

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
