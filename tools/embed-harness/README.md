# MLX embedding measurement harness

This is tooling only. It is outside the Cargo workspace and is never imported
by a Rust crate. Conversion accepts only a local model directory; it never
downloads weights. Run measurement cells only on the single-tenant main
checkout after the corpora and models are mounted.

All commands below start at the repository root unless a command explicitly
changes directory. Python module commands run from `tools/embed-harness/`, as
required by the flat `embed_harness` package layout.

## Environment and self-tests

```bash
cd tools/embed-harness
python3 -m venv .venv
.venv/bin/python -m pip install -r requirements.txt
.venv/bin/python -m pytest -q tests
cd ../..
```

The five tests use a fixed-seed, 2-layer, 16-wide synthetic BERT checkpoint
and a 20-document synthetic BEIR corpus. They do not read any external mount.

## Control arm

First convert the locally mounted M5 checkpoint. `convert` fails if the local
directory, `config.json`, or `model.safetensors` is absent.

```bash
cd tools/embed-harness
H=.venv/bin/python
$H -m embed_harness.convert \
  --source "$LOCAL_MODELS/multilingual-e5-small" \
  --out models/m5 \
  --model-id intfloat/multilingual-e5-small \
  --model-version "$M5_REVISION" \
  --pooling mean \
  --prompt-prefix 'query: ' \
  --document-prefix 'passage: ' \
  --max-tokens 512 \
  --normalize
$H -m embed_harness.encode calibrate-control \
  --model models/m5 \
  --backend mlx \
  --queries /private/tmp/beir/scifact/queries.jsonl \
  --tokens 16 \
  --n 1000 \
  --out control.json
cd ../..
```

The orchestrator separately runs the existing whole-machine non-idle CPU
calibration before these commands. Every latency cell below measures this
control before and after the target and marks the JSON `void` if either p50
is outside ±5% of `control.json`.

## H0 hybrid baseline

```bash
cargo build --release -p zeppelin-embed-bench --bin hybrid-alpha
ZE_BEIR_DIR=/private/tmp/beir \
ZE_SCIFACT_VECTORS="$RAGBENCH/scifact" \
target/release/hybrid-alpha \
  --corpus scifact \
  --vectors "$RAGBENCH/scifact" \
  --k 10 \
  --time-searches \
  --time-passes 3 \
  --load-limit 3.0 \
  --out tasks/evidence/26-lane0-h0-hybrid-baseline.md
```

Without `--time-searches`, `hybrid-alpha` performs the existing alpha and rule
sweeps without emitting latency or taint lines. `--out` contains the `DATA`,
`BUILD`, `HYBRID_ALPHA_RESULT`, `BEST_ALPHA`, `HYBRID_LATENCY`, and
`HYBRID_STAGE` lines; all other diagnostics remain on stdout.

## M cells

Set `MODEL_ID`, `LOCAL_MODEL`, `POOLING`, and `PREFIX` once per candidate.
The BERT/XLM-R converter is complete. Pooling is `mean`, `cls`, or
`last-token`.

```bash
cd tools/embed-harness
H=.venv/bin/python
$H -m embed_harness.convert \
  --source "$LOCAL_MODEL" \
  --out "models/$MODEL_ID" \
  --model-id "$HF_MODEL_ID" \
  --model-version "$HF_MODEL_REVISION" \
  --pooling "$POOLING" \
  --prompt-prefix "$PREFIX" \
  --normalize

# The two multi-million-document corpora use one fixed comparison subset.
# Every positive test qrel is retained; the remainder is selected by a
# seed-derived SHA-256 rank.  Each output carries source and output hashes.
$H -m embed_harness.evalir subset-beir \
  --source /private/tmp/beir/hotpotqa \
  --out data/beir-subsets/hotpotqa \
  --max-documents 50000 \
  --seed 20260903
$H -m embed_harness.evalir subset-beir \
  --source /private/tmp/beir/dbpedia-entity \
  --out data/beir-subsets/dbpedia-entity \
  --max-documents 50000 \
  --seed 20260903

$H -m embed_harness.encode length-distribution \
  --model "models/$MODEL_ID" \
  --backend mlx \
  --corpus \
    /private/tmp/beir/scifact/corpus.jsonl \
    /private/tmp/beir/nfcorpus/corpus.jsonl \
    /private/tmp/beir/fiqa/corpus.jsonl \
    /private/tmp/beir/trec-covid/corpus.jsonl \
    /private/tmp/beir/scidocs/corpus.jsonl \
    data/beir-subsets/hotpotqa/corpus.jsonl \
    data/beir-subsets/dbpedia-entity/corpus.jsonl \
  --out data/length-dist.json

$H -m embed_harness.encode latency \
  --model "models/$MODEL_ID" \
  --backend mlx \
  --queries /private/tmp/beir/scifact/queries.jsonl \
  --tokens 16 \
  --warmup 100 \
  --n 10000 \
  --control control.json \
  --control-model models/m5 \
  --control-queries /private/tmp/beir/scifact/queries.jsonl \
  --out "results/$MODEL_ID-latency-16.json"

$H -m embed_harness.encode latency \
  --model "models/$MODEL_ID" \
  --backend mlx \
  --queries /private/tmp/beir/scifact/queries.jsonl \
  --tokens 32 \
  --warmup 100 \
  --n 10000 \
  --control control.json \
  --control-model models/m5 \
  --control-queries /private/tmp/beir/scifact/queries.jsonl \
  --out "results/$MODEL_ID-latency-32.json"

$H -m embed_harness.encode throughput \
  --model "models/$MODEL_ID" \
  --backend mlx \
  --queries /private/tmp/beir/scifact/queries.jsonl \
  --lengths data/length-dist.json \
  --batch 32,128,512 \
  --out "results/$MODEL_ID-throughput.json"

$H -m embed_harness.evalir encode-beir \
  --model "models/$MODEL_ID" \
  --backend mlx \
  --beir-dir /private/tmp/beir \
  --corpus scifact \
  --out "vectors/$MODEL_ID/scifact"

$H -m embed_harness.evalir hybrid \
  --binary ../../target/release/hybrid-alpha \
  --beir-dir /private/tmp/beir \
  --corpus scifact \
  --vectors "vectors/$MODEL_ID/scifact" \
  --k 10 \
  --out "results/$MODEL_ID-scifact-hybrid.json"
cd ../..
```

Repeat `encode-beir` and `hybrid` for each mounted BEIR corpus. The NumPy
reference path uses the same commands with `--backend numpy`; it sets the
common BLAS/Accelerate thread-count variables to one before importing NumPy.

## Q cells

The manifest pins every MS MARCO triples file by SHA-256. Each TSV line is
exactly `query<TAB>positive<TAB>negative` with no header.

```bash
cd tools/embed-harness
H=.venv/bin/python
$H -m embed_harness.towers manifest \
  --triples "$TRAIN/msmarco-triples.tsv" \
  --out data/train.manifest.json

$H -m embed_harness.towers train \
  --cell q2 --teacher "models/$MODEL_ID" \
  --data data/train.manifest.json --seed 20260903 \
  --batch 128 --lr 0.0001 --warmup 1000 --temperature 0.05 \
  --max-wall-hours 12 --out towers/q2
$H -m embed_harness.towers train \
  --cell q3 --teacher "models/$MODEL_ID" \
  --data data/train.manifest.json --seed 20260903 \
  --batch 128 --lr 0.0001 --warmup 1000 --temperature 0.05 \
  --max-wall-hours 12 --out towers/q3
$H -m embed_harness.towers train \
  --cell q4 --teacher "models/$MODEL_ID" \
  --data data/train.manifest.json --seed 20260903 \
  --batch 128 --lr 0.0001 --warmup 1000 --temperature 0.05 \
  --max-wall-hours 12 --out towers/q4

$H -m embed_harness.towers eval \
  --teacher "models/$MODEL_ID" \
  --tower towers/q4 \
  --beir /private/tmp/beir/scifact \
  --doc-vectors "vectors/$MODEL_ID/scifact" \
  --out-vectors vectors/q4/scifact \
  --out results/q4-scifact-dense.json
$H -m embed_harness.evalir hybrid \
  --binary ../../target/release/hybrid-alpha \
  --beir-dir /private/tmp/beir \
  --corpus scifact \
  --vectors vectors/q4/scifact \
  --k 10 \
  --out results/q4-scifact-hybrid.json
cd ../..
```

Use `--tower q1`, `--tower towers/q2`, or `--tower towers/q3` for the other
query cells. `--max-steps N` may be added for an explicitly budgeted run. A
budget-cut run records `converged: null` rather than extrapolating.

## Probes

NevIR input may be JSONL with string fields `query_a`, `query_b`, `doc_a`,
and `doc_b`, or TSV with exactly those four columns and a header. The loader
fails if the path is absent.

```bash
cd tools/embed-harness
H=.venv/bin/python
$H -m embed_harness.probes \
  --teacher "models/$MODEL_ID" \
  --tower towers/q4 \
  --probe shuffle \
  --beir /private/tmp/beir/scifact \
  --doc-vectors "vectors/$MODEL_ID/scifact" \
  --out results/q4-scifact-shuffle.json
$H -m embed_harness.probes \
  --teacher "models/$MODEL_ID" \
  --tower towers/q4 \
  --probe nevir \
  --nevir "$NEVIR/nevir.jsonl" \
  --out results/q4-nevir.json
cd ../..
```

## Complete cell result

`run_cell` creates the complete null-disciplined result envelope. Populate
measured fields with `--number FIELD=FLOAT`; every remaining field stays
`null` with its explicit note. Add one `--taint` per known contaminant.

```bash
cd tools/embed-harness
.venv/bin/python -m embed_harness.run_cell \
  --cell M5-scifact \
  --model models/m5 \
  --number dense_ndcg10=0.7181 \
  --skip hybrid_ndcg10='hybrid step was not run' \
  --out results/m5-scifact-cell.json
cd ../..
```

## Intentional stubs

- ModernBERT, Gemma-3, and Qwen3 conversion raise a named
  `NotImplementedError`; no architecture is faked.
- MIRACL loading raises `NotImplementedError` until the corpus is mounted and
  its release/version contract is pinned.
- The multilingual training-set reader is a stub. MS MARCO triples are fully
  supported.
- Q5 raises `NotImplementedError` because it is explicitly owner-gated.
