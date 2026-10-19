# Zeppelin Embed for Python

Python 3.11+ bindings for the Zeppelin Embed C ABI. Platform wheels include the
native library, so a wheel installation does not require a separate Rust build.

```bash
python -m pip install zeppelin-embed
```

```python
from pathlib import Path

import numpy as np
import zeppelin_embed as ze

vectors = np.asarray([[0.0, 0.5, 1.0, 1.5]], dtype=np.float32)
with ze.open(Path("example-store")) as store:
    store.ingest([1], vectors, texts=["first document"])
    result = store.query(vector=vectors[0], text="first", k=1)
    print(result.hits[0].doc_id)
```

## Native library loading

The loader uses `ZEPPELIN_EMBED_LIBRARY` when it is set. Otherwise, an installed
wheel loads its bundled library from `zeppelin_embed/.dylibs`. Source-tree use
also recognizes the workspace's debug and release Cargo outputs for development.
An invalid override fails loudly and does not fall back to another library.

The arm64 wheel (the only published wheel, including text) requires macOS 14 or
newer on Apple silicon. Other platforms are not part of this PyPI release.

## Text API (macOS arm64)

Apple silicon wheels include the text feature and MLX's `mlx.metallib` beside
its native library. Other builds raise `ze.TextUnavailable` with
"text API is available on macOS arm64 only" from text entry points.
No separate MLX install or shader-library path is needed.

```python
import zeppelin_embed as ze

with ze.open_text("text-store", "model.zem") as store:
    store.ingest_text([7, 9], ["the bronze zeppelin", "marine biology field notes"])
    for legs in ("dense", "lexical", "hybrid"):
        result = store.query_text("bronze zeppelin", k=1, legs=legs, tier="exact")
        print(result.hits, result.diagnostics)
    store.ingest_text([7], ["bronze"], revisions=[2])  # replace the previous revision
    store.delete_text([9])  # removes every chunk belonging to document 9
    print(store.health())
    store.maintain(wall_time_ns=10_000_000, bytes=1_000_000)
```

To download and verify a model from Hugging Face, call
`ze.open_model("text-store", repo=repo, revision=commit_sha,
manifest_sha256=manifest_sha256, cache_dir=None, offline=False, progress=None)`.
All three pins are required; there is no default model until ZE-500.
The native resolver honors `ZE_MODEL_CACHE`, `ZE_MODEL_OFFLINE`, and
`ZE_MODEL_ENDPOINT`. `progress(done, total)` runs synchronously at file boundaries
and must not reenter model resolution; callback exceptions propagate after resolution.
`offline=True` verifies a cached bundle and raises `ze.NotFound` on a cache miss.

`ingest_text` and `query_text` accept `cancel_token=ze.CancelToken()` or
`deadline_ns` (a relative monotonic budget), mutually exclusive. Cancellation
raises `ze.Cancelled`; expiry raises `ze.Timeout`. Ingest may leave acknowledged
partial writes. Its report includes generation, documents, chunks, tokens, batches,
and worker completion. Set `chunk_max` and `chunk_overlap` to split long documents
into token windows. Queries return stored text, core diagnostics, backend facts,
and stage timings when the native runtime enables them. Text `maintain` executes
one budgeted native maintenance call.

Explicit `store.observed_compute_units()` loads the CoreML compute plan on demand and returns planned placement counts (not an execution trace), or `None` without a CoreML query model.

## Development

The package is built from the repository because the wheel build compiles
`crates/zeppelin-embed-ffi` with the locked Rust 1.93 dependency graph:

```bash
python -m pip wheel --no-deps ./bindings/python -w wheelhouse
```

Run all binding checks on macOS arm64, outside a process sandbox (MLX uses Metal):

```bash
cargo build --locked -p zeppelin-embed-ffi --features text
ZEPPELIN_EMBED_LIBRARY="$PWD/target/debug/libzeppelin_embed_ffi.dylib" \
  python -m pytest bindings/python/tests
```

The text tests use the committed synthetic bundle
`crates/zeppelin-embed-text/tests/fixtures/ze115/gte.zem` and a local HTTP server
for model download, verified cache reuse, and offline behavior. No real HF
ModelRef is selected yet; cached real-model CI coverage follows in ZE-500.

Build and verify the installed wheel in a fresh venv and temporary directory:

```bash
python -m pip wheel --no-deps ./bindings/python -w wheelhouse
python bindings/python/tests/verify_wheel.py wheelhouse/zeppelin_embed-*.whl
```

The verifier clears source-library overrides, copies the synthetic fixture into
the temporary directory, and tests ingest plus all three query legs using only
the installed package and its colocated shader library.

## Releasing

Manually dispatching `python-release.yml` with release tag `v<version>` builds
and verifies the macOS arm64 wheel, then uploads it with PyPI Trusted
Publishing. Configure
the PyPI publisher with owner `zepdb`, repository `zeppelin-embed`, workflow
`python-release.yml`, and environment `pypi`.

### Prepare lexical queries after open

Call `store.warm_lexical()` before exact or `last_as_prefix=True` queries to
prepare the lexical assembly and prefix vocabulary, including record-only
namespaces. Repeated calls reuse preparation; mutations invalidate it.
Optional `cancel_token` and `deadline_ns` controls are mutually exclusive.
A cancelled call may retain a complete assembly and can be retried.
