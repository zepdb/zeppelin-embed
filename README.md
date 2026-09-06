<div align="center">

<img src="assets/zeppelin-embed.png" alt="Zeppelin Embed icon" width="112">

# Zeppelin Embed

**Embedded vector, lexical, and hybrid search specialized for macOS.**

[![CI](https://github.com/zepdb/zeppelin-embed/actions/workflows/ci.yml/badge.svg)](https://github.com/zepdb/zeppelin-embed/actions/workflows/ci.yml)
[![Python](https://github.com/zepdb/zeppelin-embed/actions/workflows/python.yml/badge.svg)](https://github.com/zepdb/zeppelin-embed/actions/workflows/python.yml)
[![Crates.io](https://img.shields.io/crates/v/zeppelin-embed.svg)](https://crates.io/crates/zeppelin-embed)
[![PyPI](https://img.shields.io/pypi/v/zeppelin-embed.svg)](https://pypi.org/project/zeppelin-embed/)
[![Rust 1.93+](https://img.shields.io/badge/rust-1.93%2B-93450a.svg)](https://www.rust-lang.org)
[![License: GPL v3](https://img.shields.io/badge/license-GPLv3-blue.svg)](LICENSE)

[Quick start](#quick-start) · [Search](#one-store-three-ways-to-search) · [APIs](#language-apis) · [Durability](#durability)

</div>

Zeppelin Embed is an in-process search engine built for macOS and Apple
silicon. It keeps vectors, text, metadata, and search indexes together in a
persistent directory and serves them without a database server.

## Native search performance

| BEIR dataset | Vector p50 / p95 | Lexical p50 / p95 | Hybrid p50 / p95 |
|---|---:|---:|---:|
| FiQA | 0.06 / 0.09 ms | 0.87 / 3.13 ms | 1.24 / 3.86 ms |
| SciFact | 0.05 / 0.06 ms | 0.13 / 0.39 ms | 0.28 / 0.66 ms |
| TREC-COVID | 0.09 / 0.11 ms | 2.83 / 7.68 ms | 3.66 / 8.94 ms |
| NQ prepared prefix | 0.78 / 0.88 ms | 0.54 / 1.59 ms | 3.38 / 3.74 ms |

Measured on an Apple M3 Max using the native Rust API with warm indexes,
precomputed query vectors, preanalyzed lexical terms, graph vector search, and
`k=10`. Each value is the median of three process-level p50 or p95 results.

The Rust core provides graph and exact vector retrieval, BM25 lexical search,
and hybrid fusion over the same point-in-time snapshot. Applications supply
their own document and query vectors.

## Why Zeppelin Embed

- **Vector, lexical, and hybrid retrieval.** Use one store and one document ID
  space for vector similarity, BM25, or fused results.
- **Local and persistent.** The engine runs inside your process, recovers from
  its write-ahead log, and publishes immutable searchable generations.
- **Fast native execution.** Runtime-dispatched SIMD kernels, quantized graph
  traversal, exact rescoring, memory-mapped segments, and bounded worker pools.
- **Production controls.** Typed filters, idempotent document revisions,
  cancellation, deadlines, retention, physical purge, memory budgets, health,
  and query diagnostics.
- **Bring your own vectors.** Use embeddings from any model that produces
  compatible document and query vectors.
- **One native engine, several languages.** Rust, a versioned C ABI, Python,
  and Swift call the same storage and retrieval implementation.

## Quick start

Install the Python bindings for the native core:

```bash
python -m pip install zeppelin-embed
```

Create a store, add vectors and text, then run a hybrid query:

```python
from pathlib import Path

import numpy as np
import zeppelin_embed as ze

documents = np.asarray(
    [
        [1.0, 0.0, 0.0, 0.0],
        [0.8, 0.2, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
    ],
    dtype=np.float32,
)

with ze.open(Path("my-search-index")) as store:
    store.ingest(
        [101, 102, 103],
        documents,
        texts=[
            "fast embedded hybrid search",
            "persistent vector retrieval",
            "a completely different document",
        ],
    )

    result = store.query(
        vector=np.asarray([1.0, 0.0, 0.0, 0.0], dtype=np.float32),
        text="fast hybrid search",
        k=2,
    )

    for hit in result.hits:
        print(hit.doc_id, hit.score)
```

The example supplies its own document and query vectors. Zeppelin Embed
v0.1.0 does not bundle or download an embedding model.

The directory is the database. Reopen the same path to recover its committed
state and continue ingesting or searching.

For Rust:

```bash
cargo add zeppelin-embed
```

The core Rust API starts with [`Store`](crates/zeppelin-embed/src/lifecycle/mod.rs),
[`IngestDocument`](crates/zeppelin-embed/src/ingest/mod.rs), and
[`SearchRequest`](crates/zeppelin-embed/src/ingest/mod.rs). The Python wrapper
exposes the same lifecycle through a stable C ABI.

## One store, three ways to search

| Mode | Input | Result |
|---|---|---|
| Vector | A precomputed `float32` query vector | Approximate graph retrieval by default, with exact and scan tiers available |
| Lexical | Raw query text or a structured lexical query | BM25 ranking with term, phrase, prefix, and phonetic operators |
| Hybrid | A query vector plus text | Vector and lexical candidates fused over one pinned store generation |

Vector and hybrid graph queries use quantized traversal to select candidates and
full-precision vectors to score the retained rows. Exact search remains
available when exhaustive membership is required. Every result reports the
generation it observed, and diagnostics report the path and work that actually
ran.

## Durability

The default `Derived` durability mode is intended for indexes that can be
rebuilt from another authoritative data source. It recovers a structurally
valid store after interruption, but a power loss can discard a recently
acknowledged tail.

If Zeppelin Embed is the authoritative copy, select durable commits explicitly:

```python
with ze.open(
    "my-search-index",
    durability=ze.DurabilityMode.DURABLE,
    commit_tier=ze.CommitTier.DURABLE,
) as store:
    ...
```

Readers search a pinned generation while writes continue. One process owns the
writer lock; additional processes can open committed snapshots read-only.

## Language APIs

| API | Package or entry point | Current release targets |
|---|---|---|
| Rust core | [`zeppelin-embed`](crates/zeppelin-embed) | macOS and Linux |
| C | [`zeppelin_embed.h`](crates/zeppelin-embed-ffi/include/zeppelin_embed.h) | Static and dynamic libraries |
| Python | [`python/`](python) | macOS Apple silicon and manylinux x86-64 wheels |
| Swift | [`ZeppelinEmbed`](swift/ZeppelinEmbed) | macOS 14+ and iOS 17+ |

The C ABI uses size-versioned requests and responses, typed error codes, and
matching free functions for every callee-owned result. Python wheels include
the native core library. Swift consumes the same ABI through an actor-based
interface.

## Building from source

Zeppelin Embed uses stable Rust 1.93 or newer.

```bash
git clone https://github.com/zepdb/zeppelin-embed.git
cd zeppelin-embed
cargo test --workspace
```

Build the core C ABI library:

```bash
cargo build --release -p zeppelin-embed-ffi
```

Build a Python wheel:

```bash
python -m build --wheel python
```

## License

Zeppelin Embed is free software licensed under the
[GNU General Public License v3.0](LICENSE).
