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
