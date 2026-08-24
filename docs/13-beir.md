# Track L measurement — BEIR quality and a competitor bake-off

Quality measured 2026-08-23 and re-confirmed unchanged 2026-08-24. Latency
re-measured 2026-08-24, single-tenant, with build profiles matched between
engines; section 3 supersedes the contaminated table it replaces.

Machine: Apple M3 Max (Mac15,9), 12P+4E, 128 GB, macOS. Engine builds at
`opt-level = 3`, fat LTO, 1 CGU. **Owner decision O6 was taken on
2026-08-24: we ship speed, not size.** The `opt-level = "z"` column below is
kept as the evidence that produced that decision, not as a live option.

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
`opt-level = "z"`; both were reported, and the difference is what settled
**O6**. The shipped profile is now level 3, so the left-hand column is the
one that ships.

Mean ms per query, block-max pruned path, median of five:

| corpus | zeppelin (shipped) | tantivy | verdict | at the old `z` |
| --- | ---: | ---: | :--- | ---: |
| TREC-COVID | 4.960 | 4.880 | tie, 1.02x, inside spread | 8.040 |
| FiQA | **1.343** | 1.568 | **we win 1.17x** | 2.090 |
| NFCorpus | **0.025** | 0.170 | **we win 6.87x** | 0.040 |
| SciFact | **0.230** | 0.657 | **we win 2.86x** | 0.373 |

Read honestly:

- **We win three corpora outright and tie the fourth.** TREC-COVID at 1.02x
  sits well inside the spreads — ours 4.920-5.240, tantivy's 4.760-5.280,
  which overlap almost entirely — so it is a tie, not a win. It was 18x
  behind at the start of this work.
- **The control says the conditions were sound.** A long coverage job was
  running on another checkout during this run. Because the arms alternate,
  tantivy doubles as a control: its numbers landed within 2.8% of the
  quiesced run (4.880 against 5.020, 1.568 against 1.540, 0.170 against
  0.170, 0.657 against 0.663).
- **O6 is settled: we ship `opt-level = 3`.** At `"z"` we lost TREC-COVID by
  1.75x and FiQA by 1.52x and won only two corpora. Level 3 costs +150 KB of
  linked sections, 1,873 to 2,023 KB, against a 5,120 KB budget. The rule for
  this repository is best-and-fastest, not smallest.
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

1. **TREC-COVID, the last tie.** Block skipping is the mechanism tantivy
   wins on, and `blocks_skipped` is still zero. The canonical BMW jump is in,
   but a one-block jump still DECODES the block it lands in, so nothing is
   passed undecoded. True skipping needs the cursor to sit at a block
   boundary without decoding. Its ceiling is the measured 6-8% decode share,
   so measure before building.
2. **Indexing: 29x-36x behind, and untouched.** P3 was never attempted.
   Part of the gap is tantivy's multi-threaded writer against our
   single-threaded seal, disclosed in section 3 rather than corrected.
3. **FiQA is our weakest corpus relative to the field**, the only one where
   tantivy outranks us (0.2380 against 0.2290). Nothing has been tried.
4. **FTS5 re-run 2026-08-24, section 7.** Closed: we beat it on every
   corpus on query latency (26x-40x) and on quality; it still indexes
   faster than we do, which folds into owed item 2.
5. Multifield TREC-COVID measured 0.5906 against a 0.656 secondary target.
   The 3:1 title weighting is a guess, not Pyserini's; the weighting needs
   to be matched before that number means anything.

## 7. Addendum 2026-08-24: MAXSCORE probe order, machinery cuts

Three commits after the section-3 measurement (`db3cb24`, `164d7c6`,
`44644ac` on `fts-next`):

- **MAXSCORE probes descending by bound and tests a metadata-only block
  bound before every seek.** The old ascending order paid the long lists'
  seeks before the abandon test could spare them. Deterministic counters,
  zipf 100k six-term k=10: postings_decoded 108,452 to **60,803** (-44%),
  blocks_decoded 3,831 to **2,719** (-29%). MAXSCORE became uniformly
  cheaper, so the strategy rule re-derived to **MAXSCORE at <= 3 terms,
  WAND at >= 4** (was <= 2 / >= 3); the three dissenting 2,000-doc k=100
  cells are recorded in `strategy_rule.contract` rather than fitted.
- **Bit-exact per-posting machinery cuts**: block impact pair cached at
  decode; whole-list impact computed at seal (upper bounds stop walking
  every metadata row per query); one-run unit-weight streams skip the
  merge machinery; in-block seek landing is a binary search. Counters
  unchanged.
- **Seal stops re-decoding for union document frequencies**: the union is
  counted from the in-memory sorted lists during the seal loop.

**Quality is bit-identical**: 0.6004 / 0.2290 / 0.3183 / 0.6972 under
`ZE_BEIR_PRUNE=1`, the equivalence contract holding through all three
changes.

**Latency**, three repetitions, arms alternated within each repetition,
medians, ms/query. NOT verified single-tenant — no foreign process check
was made — so the defence is the alternation and the spreads (ours 4%,
tantivy 10% min-to-max on TREC-COVID). FTS5 is one repetition, closing
owed item 4.

| corpus | zeppelin @ release (o3) | tantivy | FTS5 | verdict vs tantivy |
| --- | ---: | ---: | ---: | :--- |
| TREC-COVID | 5.400 | **4.560** | 213.4 | tantivy 1.18x |
| FiQA | **1.323** | 1.560 | 56.9 | we win 1.18x |
| NFCorpus | **0.025** | 0.164 | 1.019 | we win 6.6x |
| SciFact | **0.237** | 0.647 | 6.25 | we win 2.7x |

TREC-COVID moved from the section-3 tie to a 1.18x loss in this run;
tantivy also ran faster than its own quiesced section-3 number (4.56
against 5.02), so machine state differs from that run in tantivy's
favour. The corpus remains the one to watch, and its query shape (long
queries, WAND) is untouched by the MAXSCORE work above.

Indexing, medians of the same repetitions, whole corpus, ms — the union
fix bought roughly 5% and the gap remains owed to P3:

| corpus | zeppelin | tantivy | ratio | FTS5 |
| --- | ---: | ---: | ---: | ---: |
| TREC-COVID | 22,178 | **670** | 33.1x | 4,711 |
| FiQA | 5,425 | **182** | 29.8x | 1,025 |
| NFCorpus | 628 | **25** | 25.1x | 81 |
| SciFact | 855 | **34** | 25.1x | 122 |

Standing, quality (unchanged from section 2): mean nDCG@10 **0.4612**
against tantivy 0.4429 and FTS5 0.4436; three corpora won outright,
FiQA still tantivy's (owed item 3).
