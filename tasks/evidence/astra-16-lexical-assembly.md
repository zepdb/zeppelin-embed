# Step 16: reuse immutable lexical contributions

Implementation: retained. Focused validation: GREEN. Matched core measurement:
complete. Full native TextStore confirmation is deferred to the shared after-17
checkpoint; broad qualification is NOT RUN. Commit: pending.

## Result and contract

One active append no longer recomputes unchanged sealed lexical row/token
counts or clones their membership bitmaps. The public Store setup benchmark
improves p95 by 59.85-75.22% across three synthetic update workloads, with exact
compared identities, revisions and score bits. No FiQA-specific policy was tuned.
Model, retrieval and worker defaults are unchanged.

LexicalIndex now shares validated live statistics objects. The lifecycle cache
owns each contribution and its memory reservation through an Arc. Active alive
summaries use exact immutable ActiveSegment identity. Publication remaps sealed
readers, so sealed reuse requires the same immutable file metadata, verified
postings length/checksum and exact current live bitmap. Normal query_postings
and query_alive validation still runs before reuse; no checksum is bypassed.
Postings/current sealed alive views remain reader-owned. Source ordinals are
rebuilt from the current snapshot, independently of contribution lookup order.
The cache sorts by segment ID and uses binary search, rather than pairwise
full contribution scans. All cached membership/summary bytes remain charged
until the final contribution owner drops, including old admitted queries.

One builder holds the cache entry lock through construction. The old entry
remains charged until successful replacement; refusal leaves it valid. An older
admitted generation may build its own answer but cannot replace a newer entry.
There is one cached assembly, no unbounded history, and no new publication/WAL
ordering or persisted/ABI change. The finite build lock is not a new end-to-end
cancellation guarantee; Step 18 owns queue/checkpoint cancellation.

## Literal RED, GREEN and failure plants

- `astra_16_active_ingest_reuses_unchanged_sealed_lexical_statistics`: actual
  RED visits 2,050 bitmap rows after one append to 2,048 sealed rows. Exact
  same command GREEN visits two rows for the new active alive/token summary;
  unchanged sealed row walks are zero. The first draft expected one visit;
  it was corrected to two and rerun to RED before production changes.
- `astra_16_alive_change_invalidates_only_affected_contribution`: after deleting
  one row from the first of two sealed segments, one contribution is reused,
  one rebuilt and exactly 31 live lengths are read. A dedicated call-site
  observer separates these statistics reads from remap/bitmap decoding work.
- `astra_16_cached_global_stats_match_exhaustive_live_model`: an independent
  literal live corpus and BM25 equation, plus fresh uncached exhaustive score
  bits, cover active append, sealed revision replacement, delete, seal and
  physical purge. No stale document/source mapping.
- `astra_16_retention_reuses_survivor_with_new_source_ordinal`: retention removes
  the first segment, reuses the surviving contribution at ordinal zero, reads
  zero live lengths and returns the correct identity and literal score.
- Three unit tests use actual admissions and deterministic barriers to prove
  old/new query lifetime accounting, delayed old generations not overwriting
  newer cache state, and failed contribution reservation leaving no new charge
  or partially published value. Test barriers release peers during unwinding.
- Two deliberate plants release a contribution reservation early and permit
  stale cache overwrite. Both named tests fail. Restored code passes all three
  unit tests. Existing assembly, vocabulary and prepared-query ownership tests
  also pass; no unfiltered adversarial campaign was run for this cache change.

Commands and per-command exits: `astra-16-raw/final-checks.json`. Final checks
execute seven new cases and five affected regressions (assembly ownership,
vocabulary lifetime, prepared bindings/budget, public cache invalidation and
field union). The existing consolidation preservation test separately passes
one case, including lexical result parity. No zero-match/ignored case is counted.
Targeted Clippy passes with the existing fts/search collapsible-if and four
reader lifetime warnings. Scoped formatting/diff checks pass.

```sh
cargo test -p zeppelin-embed --test lexical_stats_consistency astra_16_active_ingest_reuses_unchanged_sealed_lexical_statistics -- --exact
cargo test -p zeppelin-embed --test lexical_stats_consistency astra_16_
cargo test -p zeppelin-embed --lib astra_16_
cargo test -p zeppelin-embed --test consolidation consolidation_merges_every_graph_segment_into_one_and_preserves_results -- --exact
```

## Matched measurement

Host: Apple M3 Max (Mac15,9), 128 GiB RAM, macOS 27.0 / 26A5388g. Rust 1.93.0,
aarch64, opt3, fat LTO, one codegen unit, stripped symbols and panic=unwind.
Baseline is a fresh archive of `7d0f9ef`; after is that source plus this step.
Both external standalone manifests compile the same public Store example with
`core_features: []`, explicitly verified from Cargo compiler-artifact receipts.
Fault observers are absent from all wall-time binaries. The source/flags/hashes
are in `astra-16-measured/source.json`, `source.patch`, and build receipts.

Three synthetic geometries: 8,192 rows in one/eight segments and 65,536 rows
in one segment. Text cycles among literal common-term/two-token and absent-term
rows, with two-dimensional vectors. No embedding runtime, model, qrels or FiQA
index participates. Each process first times 128 warmed absent-term queries,
after 20 warmups. It then performs 84 active appends, timing absent-term setup
and a subsequent warm common-term query; the first 20 are discarded. Appends,
stats reads and serialization are outside query timing. k=10 remains fixed.
First-use cache construction is not timed; no cold-first-query speed claim.
API scope is the public core Store lexical query, not an embedding TextStore API.

Six independent processes run before/after, after/before, before/after; each
waits for one-minute host load <=3. Graph maintenance and compilation are paused
during these cells. Every process succeeds. All 11,520 returned control hits
and 4,608 timed calls are validated. Percentiles are nearest-rank per process;
reported values are medians of the three process percentiles, not pooled data.

| Rows | Segments | Query phase | Before p50 us | After p50 us | Before p95 us | After p95 us | p95 change |
| ---: | ---: | --- | ---: | ---: | ---: | ---: | ---: |
| 8,192 | 1 | warm_us | 0.500 | 0.541 | 0.500 | 0.542 | +8.40% |
| 8,192 | 1 | setup_us | 27.250 | 10.417 | 28.541 | 11.458 | -59.85% |
| 8,192 | 1 | present_us | 2.334 | 2.334 | 2.833 | 2.750 | -2.93% |
| 8,192 | 8 | warm_us | 0.667 | 0.708 | 0.709 | 0.750 | +5.78% |
| 8,192 | 8 | setup_us | 23.042 | 5.208 | 23.708 | 5.875 | -75.22% |
| 8,192 | 8 | present_us | 4.041 | 4.042 | 4.417 | 4.458 | +0.93% |
| 65,536 | 1 | warm_us | 0.208 | 0.208 | 0.209 | 0.250 | +19.62% |
| 65,536 | 1 | setup_us | 181.458 | 53.292 | 191.792 | 54.125 | -71.78% |
| 65,536 | 1 | present_us | 2.084 | 2.083 | 2.500 | 2.459 | -1.64% |

`setup_us` is the first query after mutation; `present_us` is the subsequent
warm common-term query; `warm_us` is the initial repeated absent-term control.
The warm absent p95 increases exceed 5% in relative terms and are retained as
a negative result. Inspection shows each absolute increase is only 0.041-0.042
microseconds, one approximate hardware timer tick; process ranges are in the
aggregate. The 65,536-row warm p50 remains 0.208 us. These data do not prove
zero warm overhead, and no compiler/layout tuning was done to erase the tick.
The retained benefit is 17-138 us less post-mutation setup and zero unchanged
statistics walks. Common-term query p95 ranges from -2.93% to +0.93%.

| Rows | Segments | Before final cache bytes | After final cache bytes | Added bytes |
| ---: | ---: | ---: | ---: | ---: |
| 8,192 | 1 | 66,496 | 66,816 | +320 |
| 8,192 | 8 | 18,432 | 19,920 | +1,488 |
| 65,536 | 1 | 66,496 | 66,816 | +320 |

Process maximum RSS medians are 75,546,624 bytes before and 73,957,376 after;
do not interpret this variability as a cache memory saving. Actual retained
cache bytes increase as shown. Per-call sampled cache maxima equal final
values in this screen; they do not measure the transient old/new construction
overlap. The lifetime tests separately prove both live contributions stay
charged through eviction and release. First-use allocation and retained old
vocabulary/assembly overlap can increase peak reservation pressure.

```sh
python3 tasks/evidence/astra-16-measured/build.py
python3 tasks/evidence/astra-16-measured/run.py
python3 tasks/evidence/astra-16-measured/analyze.py
```

Build commands, CARGO_TARGET_DIR, binary hashes and feature sets are in
`build-receipts.json`; source archive location is in `source.json`. The completed
build reused `/var/folders/z_/fscz84rs53z5_2klsmkmv2vw0000gn/T/ze-astra16-37c1tlg6`
after fixing a harness-only DocId formatting compile error. `attempt0/` preserves
failed build/source receipts. `run-receipts.json` records exact argv, resource
maxima, loads and exits. Raw results are `{before,after}-rep{1,2,3}/results.json`;
`aggregate.json` retains all per-process values. Fresh runs must use fresh
artifact/output directories; completed indexes and receipts are preserved.

## Failures and remaining work

An early lifetime test found ASTRA-ISSUE-018: public stats rejects retained old
active generations because its unchanged invariant counts only current active
buffers against all active reservations. This is logged separately. Exact
internal accounting audits prove the contribution lifetime; no reservation is
dropped to make public stats appear consistent. Initial compile/signature and
barrier-test errors are preserved and do not count as intended product RED.
Vocabulary/phonetic rebuild cost after mutations (ISSUE-015) is not fixed here.
End-to-end native confirmation and lexical integration belong to the after-17
checkpoint. No whole-suite, coverage, or long-campaign qualification is claimed.
