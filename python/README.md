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

The v0.1.0 wheel pipeline builds the core C ABI for macOS 11 or newer on Apple
silicon and for x86-64 Linux using a manylinux image. Windows, Intel macOS, and
Linux AArch64 wheels have not been qualified.

The bundled wheel library does not enable the optional Rust `text` feature.
`open_text` therefore requires a compatible feature-enabled library supplied
through `ZEPPELIN_EMBED_LIBRARY`.

## Development

The package is built from the repository because the wheel build compiles
`crates/zeppelin-embed-ffi` with the locked Rust 1.93 dependency graph:

```bash
python -m build --wheel python
```

Run the core binding checks against an explicit development library:

```bash
cargo build --locked -p zeppelin-embed-ffi
ZEPPELIN_EMBED_LIBRARY="$PWD/target/debug/libzeppelin_embed_ffi.dylib" \
  python -m pytest python/tests --ignore=python/tests/test_text_api.py
```

On Linux, use `libzeppelin_embed_ffi.so` in that command. The text API test also
requires the repository's baked model fixture and a library built with
`--features text`.
