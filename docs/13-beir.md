# Track L measurement — BEIR quality and a competitor bake-off

Date: 2026-08-23. Machine: Apple M3 Max (Mac15,9), 12P+4E, 128 GB, macOS.
Engine build: `--release` (`opt-level="z"`, fat LTO, 1 CGU).

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

## 3. Latency — CONTAMINATED, read as a shape not a number

**This machine was not single-tenant during these runs.** Other agents were
active. Every number below is an upper bound on our latency and an unfair
comparison to the competitors, who were measured under the same noise. They
are recorded to show the *shape* of the gap, not to claim a result. The
single-tenant re-run is owed.

Mean ms per query, total wall time over judged queries divided by count:

| corpus | zeppelin (exhaustive) | zeppelin (pruned) | tantivy | FTS5 |
| --- | ---: | ---: | ---: | ---: |
| TREC-COVID | 87.6 | 91.6 | **4.8** | 209.8 |
| FiQA | 8.9 | 11.0 | **1.6** | 57.0 |
| NFCorpus | 0.20 | 0.49 | **0.19** | 1.11 |
| SciFact | 1.49 | 1.52 | **0.69** | 6.44 |

Index build, whole corpus, ms:

| corpus | zeppelin | tantivy | FTS5 |
| --- | ---: | ---: | ---: |
| TREC-COVID | 42,739 | **657** | 4,584 |
| FiQA | 10,020 | **183** | 1,033 |
| NFCorpus | 1,190 | **22** | 89 |
| SciFact | 1,630 | **41** | 132 |

Read honestly:

- **We beat SQLite FTS5 on both axes** — better nDCG on 3 of 4 corpora and
  2.4x to 6.4x lower query latency.
- **tantivy beats us on speed by 4x to 18x**, worst on the largest corpus.
- **tantivy builds indexes 25x to 65x faster.**

## 4. The bad finding: pruning is not currently faster

Routing the same queries through task 14's block-max pruning
(`ZE_BEIR_PRUNE=1`) produces **bit-identical nDCG on all four corpora** —
0.6004 / 0.2290 / 0.3183 / 0.6972, unchanged. That is the equivalence
contract holding on 171,332 real documents, far past anything the property
tests reach, and it is the strongest confirmation available that the
skipping logic is correct.

It is also **slower than the exhaustive scorer it replaces** (FiQA 11.0 ms
against 8.9; NFCorpus 0.49 against 0.20).

The cause is not the pruning algorithm. It is that `search_pruned`
recomputes, on every query, what the persisted format already stores:

- `build_block_bounds` re-derives every block maximum per term per query,
  scoring every posting to do it — strictly more work than the exhaustive
  scan it is trying to avoid;
- the per-row weighted length array is rebuilt per segment per query,
  which is `O(row_count)` before a single posting is examined.

Task 13 *writes* `u8` block maxima into the posting format and freezes them
in a golden. Task 14 never reads them, because the store wiring that would
put postings into segment region kind 6 and read them back does not exist
yet. Until that lands, pruning pays full price for bounds it should be
loading from disk.

This is the single highest-value item in the remaining work, and it changes
the priority order: **wiring the persisted format into the query path comes
before NEON kernel tuning**, because no decode kernel helps a path that
recomputes its own metadata.

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

`tools/lexical-bakeoff/` is its own workspace, excluded from the engine's,
exactly as the fuzz workspace is: tantivy pulls `rayon`, `serde_json`, and
`zstd-sys`, all on the engine's absolute blacklist. `cargo deny` and the
engine `Cargo.lock` never see them.

## 6. Owed

1. Single-tenant re-run of every latency number here.
2. Wire the persisted postings region into the query path so pruning reads
   stored block maxima instead of recomputing them (see §4).
3. Then the NEON decode roofline campaign (`docs/14-pruning.md`).
4. Indexing throughput: 25x-65x behind tantivy is a real gap and has had no
   attention at all.
5. Multifield TREC-COVID measured 0.5906 against a 0.656 secondary target.
   The 3:1 title weighting is a guess, not Pyserini's; the weighting needs
   to be matched before that number means anything.
