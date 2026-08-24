# Track L measurement — BEIR quality and a competitor bake-off

Quality measured 2026-08-23 and re-confirmed unchanged 2026-08-24. Latency
re-measured 2026-08-24, single-tenant, with build profiles matched between
engines; section 3 supersedes the contaminated table it replaces.

Machine: Apple M3 Max (Mac15,9), 12P+4E, 128 GB, macOS. Engine build for
quality: `--release` (`opt-level="z"`, fat LTO, 1 CGU); for latency, both
`--profile bench` (`opt-level=3`) and the shipped `--release`, because the
difference is a live owner decision (O6).

## 1. The gate: GREEN on all four corpora

Task 13's exit criterion is nDCG@10 within 2 points absolute of the
published Pyserini flat-BM25 numbers. Datasets are the standard BEIR
distributions fetched from `public.ukp.informatik.tu-darmstadt.de`.

| corpus | docs | queries | Pyserini (published) | zeppelin-embed | delta |
| --- | ---: | ---: | ---: | ---: | ---: |
| TREC-COVID | 171,332 | 50 | 0.595 | **0.6004** | +0.0054 |
| FiQA | 57,638 | 648 | 0.236 | **0.2290** | -0.0070 |
| NFCorpus | 3,633 | 323 | 0.322 | **0.3183** | -0.0037 |
| SciFact | 5,183 | 300 | 0.679 | **0.6972** | +0.0182 |

Command:

```bash
ZE_BEIR_DIR=<dir> cargo test --release -p zeppelin-embed-bench \
    --test beir_gate flat_bm25 -- --ignored --nocapture
```

The targets are the **flat** column of `research/02a:267`. The task-13 spec
mixed columns, quoting SciFact 0.665 and NFCorpus 0.325 from the multifield
column; those are corrected here, and correcting SciFact upward from 0.665
to 0.679 made the gate *harder*, not easier.

## 2. Competitor bake-off, same corpora, same qrels

Every engine indexes `title` and `body` as two fields and retrieves top-10.
Each harness recomputes nDCG@10 independently, so a bug in one evaluator
cannot flatter two engines. Negative qrels grades (`-1`, two of them in
TREC-COVID) are clamped to zero in all three harnesses.

| corpus | Pyserini | **zeppelin-embed** | tantivy 0.22.1 | SQLite FTS5 3.54 |
| --- | ---: | ---: | ---: | ---: |
| TREC-COVID | 0.595 | **0.6004** | 0.5962 | 0.5648 |
| FiQA | 0.236 | **0.2290** | 0.2380 | 0.2342 |
| NFCorpus | 0.322 | **0.3183** | 0.3029 | 0.3072 |
| SciFact | 0.679 | **0.6972** | 0.6346 | 0.6683 |
| **mean** | 0.4580 | **0.4612** | 0.4429 | 0.4436 |

We lead the mean, and win outright on three of four corpora. tantivy wins
FiQA. The SciFact margin over tantivy (+0.063) is the largest single gap in
the table and is plausibly the analysis pipeline: SciFact is scientific
prose full of hyphenated terms, and the word-delimiter decomposition that
keeps `SARS-CoV-2` whole *and* emits its parts is exactly what that corpus
rewards.

Not corrected, and reported instead: tantivy ships no English stopword list
(tantivy#2595) and FTS5's `unicode61` neither stems nor removes stopwords.
Those are real properties of the competitors, not harness defects.

## 3. Latency — SINGLE-TENANT, matched build profiles

Superseded measurement. The previous table was taken on a contended machine
AND compared a size-optimized engine against a speed-optimized tantivy; both
defects are fixed here.

**Method.** Five repetitions, arms interleaved *within* each repetition so
any drift hits both equally, median reported. Machine quiesced first: the
foreign 100%-CPU process that spoiled the earlier run was gone, Gatekeeper
had finished scanning the fresh binaries (`syspolicyd` at 1%), and no user
process exceeded 15%. Load average during the runs is largely self-inflicted
— tantivy's writer is multi-threaded — so the defence of this table is the
**symmetric spread**: 5-9% min-to-max on both arms, on every corpus.

**Fairness.** Both engines build at `opt-level = 3`, `lto = "fat"`,
`codegen-units = 1`. Both search single-threaded: tantivy's default
`Searcher::search` uses the single-thread executor. The engine is run at
`--profile bench` because the shipped `[profile.release]` is still
`opt-level = "z"`; both are reported below, because the difference is a
live owner decision (**O6**).

Mean ms per query, block-max pruned path, median of five:

| corpus | zeppelin @ o3 | tantivy | verdict | zeppelin @ shipped `z` |
| --- | ---: | ---: | :--- | ---: |
| TREC-COVID | 5.580 | **5.020** | tantivy 1.11x | 8.780 |
| FiQA | 1.489 | 1.540 | tie (within spread) | 2.336 |
| NFCorpus | **0.034** | 0.170 | **we win 5.00x** | 0.056 |
| SciFact | **0.250** | 0.663 | **we win 2.65x** | 0.383 |

Read honestly:

- **We win two corpora outright, tie one, and lose one.** The TREC-COVID gap
  is **1.11x**, down from 18x. FiQA at 1.03x sits inside the 6-8% spread and
  is a tie, not a win.
- **O6 is now load-bearing.** At the shipped `opt-level = "z"` we lose
  TREC-COVID by 1.75x and FiQA by 1.52x and win only two corpora. Level 3
  costs a measured +128 KB against a 5,120 KB budget currently using
  1,873 KB. The speed claim above is not shippable until O6 is taken.
- **Indexing is unchanged and still far behind**, 29x to 36x. tantivy's
  writer is multi-threaded and our seal is single-threaded, which is part of
  that gap and is disclosed rather than corrected; P3 was never attempted.

| corpus | zeppelin | tantivy | ratio |
| --- | ---: | ---: | ---: |
| TREC-COVID | 23,429 | **660** | 35.5x |
| FiQA | 5,625 | **174** | 32.3x |
| NFCorpus | 672 | **23** | 29.2x |
| SciFact | 909 | **31** | 29.3x |

## 4. Pruning is now faster than the scan, and still bit-identical

Routing the same queries through task 14's block-max pruning
(`ZE_BEIR_PRUNE=1`) produces **bit-identical nDCG on all four corpora** —
0.6004 / 0.2290 / 0.3183 / 0.6972, unchanged. That is the equivalence
contract holding on 171,332 real documents, far past anything the property
tests reach, and it is the strongest confirmation available that the
skipping logic is correct.

The inversion recorded here previously is **fixed**. Pruning was slower than
the exhaustive scorer because `build_block_bounds` scored every posting in
order to bound it, doing strictly more work than the scan it avoided. Bounds
are now read from the persisted impact pairs at O(blocks). Measured on the
same run, pruned against exhaustive: TREC-COVID 5.58 against 11.60 ms, FiQA
1.49 against 2.86.

## 5. Reproducing

```bash
# datasets (~280 MB)
for c in scifact nfcorpus fiqa trec-covid; do
  curl -O "https://public.ukp.informatik.tu-darmstadt.de/thakur/BEIR/datasets/$c.zip"
  unzip -q "$c.zip"
done

# this engine
ZE_BEIR_DIR=<dir> cargo test --release -p zeppelin-embed-bench \
    --test beir_gate flat_bm25 -- --ignored --nocapture

# tantivy — isolated workspace, never enters the engine Cargo.lock
cargo run --release --manifest-path tools/lexical-bakeoff/Cargo.toml -- <dir> scifact

# SQLite FTS5
python3 tools/lexical-bakeoff/fts5_bakeoff.py <dir> scifact
```

The engine's latency arm is run from the compiled test binary directly, at
`--profile bench`, so tantivy is not compared against a size-optimized
build:

```bash
cargo test --profile bench -p zeppelin-embed-bench --test beir_gate --no-run
ZE_BEIR_PRUNE=1 ZE_BEIR_DIR=<dir> \
    ./target/release/deps/beir_gate-<hash> flat_bm25 --ignored --nocapture
```

`tools/lexical-bakeoff/` is its own workspace, excluded from the engine's,
exactly as the fuzz workspace is: tantivy pulls `rayon`, `serde_json`, and
`zstd-sys`, all on the engine's absolute blacklist. `cargo deny` and the
engine `Cargo.lock` never see them.

## 6. Owed

1. **O6: the workspace `opt-level`.** Section 3 shows the speed result is
   not shippable at `opt-level = "z"`. This is the one decision standing
   between the measured numbers and the shipped ones.
2. **TREC-COVID, the last 1.11x.** Block skipping is the mechanism tantivy
   wins on and `blocks_skipped` is still zero on synthetic corpora. WAND's
   block-max refinement advances one posting where canonical BMW jumps to
   the shallowest block end; that is the next lever and it is guarded by
   `prop_pruned_topk_equals_exhaustive`.
3. **Indexing: 29x-36x behind, and untouched.** P3 was never attempted.
   Part of the gap is tantivy's multi-threaded writer against our
   single-threaded seal, disclosed in section 3 rather than corrected.
4. **FTS5 was not re-run** on 2026-08-24. Its column in section 3 is
   dropped rather than carried over from the contaminated table; the
   earlier finding that we beat it on both axes is unchallenged but is now
   older than the rest of the record.
5. Multifield TREC-COVID measured 0.5906 against a 0.656 secondary target.
   The 3:1 title weighting is a guess, not Pyserini's; the weighting needs
   to be matched before that number means anything.
