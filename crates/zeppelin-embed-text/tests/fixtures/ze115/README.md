# Small runtime contract fixtures

These fixtures are invented arithmetic models. They exercise real backend
implementations without downloading trained models. They make no production
embedding accuracy, cross-model compatibility, or accelerator placement claim.

`generate.py` writes two explicit `.zem` bundles (9,316 and 9,132 bytes) and
`reference.rs`. Each model has one transformer block, hidden width 4, one
attention head, intermediate width 6, token-type embeddings and a 3-coordinate
dense head. BERT also has position embeddings; GTE uses RoPE and a gated MLP.
Weights come from a fixed integer formula. The reference uses independent
PyTorch CPU operations in float64, including PyTorch attention, layer norm and
GELU; no values are recorded from the MLX implementation under test. Rust tests
compare mean/CLS/last pooling and 1/3-coordinate outputs on CPU/GPU with two
ragged rows and a 33-row batch, using an absolute tolerance of 0.00002.

Generation used Python 3.11.8, torch 2.12.1, xxhash 3.8.0, and rustfmt 1.8.0 (Rust 1.93.0).
These are offline fixture tools, not dependencies added to the Rust workspace.
Run `python generate.py` in a tool environment with torch and xxhash installed.
The test suite itself reads the committed bytes and has no Python dependency.

`generate_coreml.py` uses Python's standard library to encode `tiny.mlmodel`,
a 150-byte specification-version-4 model. Two INT32 inputs of exact shape
`[1,2]`, `input_ids` and `attention_mask`, feed an Add layer whose FLOAT32 output
is named `embedding`. Its literal contract is `embedding[i] = ids[i] + mask[i]`.
The tests compile this portable source with the platform `xcrun coremlcompiler`
in a temporary directory, then use the actual Rust/CoreML native bridge. No
host-specific compiled model is checked in. The runtime converts positive mask
values to one and other values to zero; tests check exact output for multiple
rows and all four requested compute policies. Policy identity does not prove
which hardware performed inference.

The hand-written protobuf encoder follows these Apple schema definitions at
commit `01788ff832317a31a14053a05eab70127b14296d`; no Apple implementation source
or model weights are copied:

- [Model.proto](https://github.com/apple/coremltools/blob/01788ff832317a31a14053a05eab70127b14296d/mlmodel/format/Model.proto)
- [FeatureTypes.proto](https://github.com/apple/coremltools/blob/01788ff832317a31a14053a05eab70127b14296d/mlmodel/format/FeatureTypes.proto)
- [NeuralNetwork.proto](https://github.com/apple/coremltools/blob/01788ff832317a31a14053a05eab70127b14296d/mlmodel/format/NeuralNetwork.proto)

Rebuild with `python3 generate_coreml.py`. Production `.zem` reference fixtures
and production CoreML placement fixtures remain separate ignored tests; these
small fixtures do not replace that qualification.
