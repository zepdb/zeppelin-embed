# Zeppelin Embed

Zeppelin Embed is the fastest and most accurate in-process search engine built
for macOS and Apple silicon. It stores vectors, text, metadata, and search
indexes in one persistent directory and supports vector, lexical, and hybrid
retrieval through a native Rust API.

```sh
cargo add zeppelin-embed
```

Applications provide their own document and query vectors. The engine owns the
persistent store, append-only ingestion, point-in-time snapshots, filtering,
and retrieval.

Run the bundled example after cloning the repository:

```sh
cargo run --release -p zeppelin-embed --example five_vectors_search
```

See the [project README](https://github.com/zepdb/zeppelin-embed) for benchmark
results, storage details, and the complete native API quickstart.

Licensed under [GPL-3.0-only](https://www.gnu.org/licenses/gpl-3.0.html).
