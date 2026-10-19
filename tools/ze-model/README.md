# Release model files (ZE-498)

Use Python 3.11.15 with `requirements-release.txt` (exact versions from the
provided model venv freeze, plus its installed `xxhash==4.0.1`). The build needs
macOS, Xcode's `xcrun coremlcompiler`, and the offline source pack. No download
is performed by `release`.

```sh
PYTHON=/Users/aghatage/Documents/code/zeppelin-embed-models/.venv/bin/python
PACK=/Users/aghatage/Documents/code/zeppelin-embed-models/zeppelin-embed-clean71-v1-arctic-m-v2
STEM=zeppelin-embed-clean71-v1-arctic-m-v2
RELEASE=/Users/aghatage/Documents/code/zeppelin-embed-models/release
"$PYTHON" tools/ze-model/ze-model release --pack "$PACK" \
  --out "$RELEASE/$STEM-fp16-coreml-int8" --name "$STEM" \
  --document-precision fp16 --query-coreml-precision int8
```

Each output directory must be absent or empty. It contains only `<name>.zem`,
`<name>.mlmodelc/`, `zeppelin-model.json`, `SHA256SUMS`, and a small `README.md`
stub that is replaced by the published model card before upload. The published
repo is `aghatage/zeppelin-embed-clean71-arctic-m-v2` at revision
`d64b817c4021a3f1b9bded3152d9afdf9ac813e7`. The manifest lists the bundle
and every compiled model file individually, sorted by relative path. The model
card is excluded from the manifest. SHA256SUMS covers all shipped files except
itself, including the manifest and model card. Both manifests hash actual disk
bytes after compilation; they never reuse pack checksums. The command prints
bundle and compiled-query byte totals and records them in the model card.

`release` calls the existing `bake` and `coreml-export` functions. It reads tower
paths/IDs and the query prefix from `PAIR.json`, pins query/document limits to
96/8192, and preserves alpha 0.5. The pair alignment identity is SHA-256 of the
exact `PAIR.json` bytes. Temporary source packages and exporter receipts do not
ship. Existing release files are never overwritten.

`bake` and `release` accept `--query-mlx-precision fp32|fp16`, defaulting to
`--document-precision`. `release --query-coreml-precision fp16|int8` and
`coreml-export --precision fp16|int8` default to fp16. INT8 applies symmetric,
per-channel weight-only linear quantization; activations and output remain FP16.
The fixed 96-token release query uses CPU_AND_NE for both choices.

The bundle retains MLX query weights at the selected precision. CoreML is
preferred, but `ZE_QUERY_COREML=off` and an absent companion select the separate
query MLX tower. Retaining its tensors preserves this supported path and bundle digest
validation. A discovered CoreML model that fails to load is a hard error.

For identical pack bytes and precision, `.zem` is deterministic. CoreML compiled
bytes may vary through metadata map serialization order, conversion dates, and
analytics data. Absolute build paths are excluded from model metadata. The
manifest and SHA256SUMS are always regenerated from the actual output bytes;
they change when compiled bytes change.
