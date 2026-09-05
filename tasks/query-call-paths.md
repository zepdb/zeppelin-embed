# Query call paths, entry point to bytes on disk

Repository HEAD `d74ca4b` ("Make query-path checksum verification opt-in").
Every line number below was read out of the working tree at that commit. This
document is a map only: no timers were added, no code was changed, nothing was
profiled. It exists so the next task can instrument systematically.

Where a dispatch could not be resolved statically it is marked **GAP** rather
than guessed.

Revision check, 2026-09-05: this remains the historical `d74ca4b` map. Both
`cf312af` (clean71 experiment) and `818a433` (main) now call
`query_bit4_codes` / `query_bit4_factors` from the sealed scan and cache the
sealed vector norm enclosure. The repeated Bit4 hashes and norm walk described
in sections 4.4, 4.5 and 11 are already addressed. The measured dense scan uses
12 workers; its remaining 2.30 ms retrieval span has not yet been split into
dispatch, scoring and merge. See the
[query-budget evidence](evidence/query-api-under-1ms.md) before treating an old
amplification finding or latency here as a current optimization opportunity.

---

## 1. Chain index

| # | Entry point | Chain begins at |
| --- | --- | --- |
| 1 | `TextStore::open` | `crates/zeppelin-embed-text/src/ingest.rs:251` |
| 2 | `TextStore::ingest_text` | `crates/zeppelin-embed-text/src/ingest.rs:363` |
| 3 | `TextStore::ingest_text_serialized` | `crates/zeppelin-embed-text/src/ingest.rs:374` |
| 4 | `TextStore::ingest_text_controlled` | `crates/zeppelin-embed-text/src/ingest.rs:443` |
| 5 | `TextStore::delete_text` | `crates/zeppelin-embed-text/src/ingest.rs:723` |
| 6 | `TextStore::query_text` — `Legs::Dense` | `crates/zeppelin-embed-text/src/ingest.rs:778` |
| 7 | `TextStore::query_text` — `Legs::Lexical` | `crates/zeppelin-embed-text/src/ingest.rs:777` |
| 8 | `TextStore::query_text` — `Legs::Hybrid` (default) | `crates/zeppelin-embed-text/src/ingest.rs:782` |
| 9 | `TextStore::health` | `crates/zeppelin-embed-text/src/ingest.rs:304` |
| 10 | `TextStore::maintain` | `crates/zeppelin-embed-text/src/ingest.rs:309` |
| 11 | `TextStore::maintain_to_completion` | `crates/zeppelin-embed-text/src/ingest.rs:325` |
| 12 | `TextStore::close` | `crates/zeppelin-embed-text/src/ingest.rs:797` |
| 13 | `Store::search` | `crates/zeppelin-embed/src/lifecycle/mod.rs:3017` |
| 14 | `Store::search_lexical` | `crates/zeppelin-embed/src/lifecycle/mod.rs:3079` |
| 15 | `Store::search_lexical_structured` | `crates/zeppelin-embed/src/lifecycle/mod.rs:3190` |
| 16 | `Store::search_hybrid` | `crates/zeppelin-embed/src/lifecycle/mod.rs:3331` |
| 17 | `Store::search_hybrid_structured` | `crates/zeppelin-embed/src/lifecycle/mod.rs:3350` |
| 18 | `Store::stored_text` | `crates/zeppelin-embed/src/lifecycle/mod.rs:3149` |
| 19 | `Store::health` | `crates/zeppelin-embed/src/diag.rs:1118` |
| 20 | `Store::maintain` | `crates/zeppelin-embed/src/tier/maintain.rs:199` |
| 21 | C ABI `ze_text_query` | `crates/zeppelin-embed-ffi/src/lib.rs:969` |
| 22 | C ABI `ze_query` (3 modes) | `crates/zeppelin-embed-ffi/src/lib.rs:1468` |
| 23 | C ABI `ze_search` | `crates/zeppelin-embed-ffi/src/lib.rs:1345` |
| 24 | C ABI `ze_text_open` / `ze_text_ingest` / `ze_text_maintain` / `ze_close` | `crates/zeppelin-embed-ffi/src/lib.rs:831` / `:864` / `:929` / `:1119` |
| 25 | C ABI `ze_maintain` | `crates/zeppelin-embed-ffi/src/lib.rs:1932` |

22 distinct execution chains (entries 2/3/4 share one write pipeline shape;
entries 16/17 share `search_hybrid_inner`).

---

## 2. The measured cell

### 2.1 Exact command

```sh
cargo build --release -p zeppelin-embed-bench --bin text-user-bench --features text

./target/release/text-user-bench steady \
  --beir-root /private/tmp/claude-501/-Users-aghatage-Documents-code-zeppelin-embed/1aa620b1-d541-4d1e-8132-57182249f51d/scratchpad \
  --corpus fiqa \
  --bundle /private/tmp/ze-model-bundles-v2-c1/leaf-v1.5-pair.zem \
  --store <scratch>/store-graph \
  --legs {dense|lexical|hybrid} --tier unset \
  --rounds 1 --warm 5 --k 10 --seed 0x5eed \
  --out <path>.json
```

Swap `--store` to `<scratch>/store-scan` for the scan-tier column. `steady`
drives `TextStore::query_text` — the public API — over 648 FiQA test queries
and emits `latencies_ms`, `ndcg_at_10` and `rankings`. Both stores are
tier-asserted with `text-user-bench verify --expect graph|scan`. The ANE query
tower was active through the sibling `leaf-v1.5-pair.mlmodelc`
(`discover_query_coreml`, `crates/zeppelin-embed-text/src/ingest.rs:1262`).

The `--tier unset` column matters: it means `QueryOptions.tier == None`, which
is **not** `SearchTier::Auto`. See §7.1.

### 2.2 Timings being explained

| leg | graph store | scan store | ratio |
| --- | ---: | ---: | ---: |
| dense | 1.055 ms | 4.542 ms | 4.3x |
| lexical | 1.414 ms | 1.389 ms | 1.0x |
| hybrid | 2.899 ms | 32.327 ms | 11.1x |

### 2.3 Store geometry (both stores)

| fact | value | source |
| --- | ---: | --- |
| rows | 58,980 | measured |
| dims | 768 | measured |
| sealed segments | 1 | measured |
| active segment | empty after `maintain_to_completion` | `ingest.rs:688` |
| `meta().scheme` | 4 (Bit4) | implied by region sizes below |
| scan-store segment file | 279,530,176 B | measured |
| graph-store segment file | 324,843,008 B | measured |
| graph node-block region (delta) | 45,312,832 B | measured |

Derived region sizes at 58,980 rows x 768 dims (all figures below are
arithmetic on those two numbers plus the frozen layouts, not measurements):

| `RegionKind` | id | bytes | formula |
| --- | ---: | ---: | --- |
| `VectorRescore` (f32) | 5 | 181,186,560 | `58,980 * 768 * 4` |
| `VectorCodes` (Bit4) | 3 | 22,648,320 | `58,980 * ceil(768/2)` |
| `VectorFactors` (Bit4) | 4 | 707,760 | `58,980 * 12` |
| `DocumentVersions` | 12 | 1,415,520 | `58,980 * 24` |
| `GraphNodeBlocks` | 7 | 45,296,768 + align | `58,980 * 768 + 128` trailer |
| `Postings` + `StoredText` + `Alive` + `ChecksumTable` | 6/14/2/11 | ~73.5 MB remainder | — |

Graph stride derivation: `round_up_128(ceil(768/2) + 16 + 4 * max_degree)`
with the Angular profile's `r_max = 64` (`crates/zeppelin-embed/src/graph.rs:96`)
gives `round_up_128(384 + 16 + 256) = round_up_128(656) = 768` bytes per node.
`58,980 * 768 + 128 = 45,296,768`; the measured 45,312,832 B delta exceeds that
by 16,064 B, which is region 16-KB alignment padding. Consistent, not proven.

### 2.4 Measured per-query volumes, placed at their hops

| volume | value | hop it belongs to |
| --- | ---: | --- |
| graph candidates scored | 4,223 | `score_and_push_group`, `graph/search.rs:1642` |
| graph candidates rescored | 204 | `rescore_top_k`, `graph/search.rs:1523` |
| effective ef | 204 | `effective_ef`, `graph/search.rs:332` (Angular = `4 * k`, `k = 51`) |
| segments traversed | 1 | `traverse_segment_graph`, `lifecycle/mod.rs:5178` |
| dense `dims_touched` @ width 51 | 3,399,936 | `= (4,223 + 204) * 768`, `graph/search.rs:1560` |
| dense `bytes_read` @ width 51 | 2,298,996 | `rescored.bytes.total()`, `graph/search.rs:1564` |
| lexical `docs_evaluated` k=10/51/400 | 1,427 / 2,600 / 6,804 | `search_allow_list_driven_controlled_with_branch`, `fts/search.rs:564` |
| lexical `postings_decoded` k=10/51/400 | 7,005 / 10,997 / 21,721 | `fts/search.rs:525` |
| lexical `blocks_decoded` at every k | **818** | `fts/search.rs:544` |
| lexical `blocks_skipped` at every k | **0** | `fts/search.rs:547` |
| lexical, whole-document query | docs 4,780–13,416; postings 86,438–194,109; blocks_decoded 7,421; blocks_skipped 0 | same hops |
| hybrid round 0 | window 50, vector_returned 100, lexical_returned 50, cross_filled_vector 50, cross_filled_lexical 0, rounds 1, `StableBound` | `build_round`, `lifecycle/hybrid.rs:96` |
| query tower | 768-d output from 64-token padded input | `QueryRuntime::embed`, `ingest.rs:1318` |

**Reads far more than it returns.** Flagged explicitly at each site below with
the marker **[AMPLIFY]**. The clearest case is the lexical leg:
`blocks_decoded` is flat at 818 from k=10 to k=400 while `blocks_skipped` stays
at 0. WAND/block-max machinery is engaged and never skips a block — the leg
decodes the same 818 blocks regardless of how many documents the caller wants,
and returns 10 of the 1,427 documents it evaluates at k=10. Raising k 40x
changes decoded postings by only 3.1x, so the cost is dominated by a fixed
per-term posting walk, not by k.

---

## 3. Chain 6 — `query_text(Legs::Dense)`

```
TextStore::query_text                        text/src/ingest.rs:763
  guard k == 0                               text/src/ingest.rs:764
  match options.legs -> Legs::Dense          text/src/ingest.rs:778
  TextStore::query_vector                    text/src/ingest.rs:789
    Bundle::tokenize_query                   text/src/bundle.rs:366
      Tokenizer::encode_batch                (bundle-owned tokenizer)
        -> TokenBatch, 64 tokens after CoreML padding
    RuntimeClient::embed(TowerRole::Query)   text/src/ingest.rs:1449
      >>> THREAD HOP: mpsc::SyncSender, cap 2                text/src/ingest.rs:1367
      >>> runs on named thread "ze-text-embed"               text/src/ingest.rs:1370
      RuntimeSet::embed                      text/src/ingest.rs:1341
        QueryRuntime::embed                  text/src/ingest.rs:1318
          CoreMlRuntime: TokenBatch::padded_to(sequence)     text/src/ingest.rs:1325
          CoreMlRuntime::embed_batch         (ANE)           text/src/ingest.rs:1326
          -- or MlxRuntime::embed_batch when no .mlmodelc    text/src/ingest.rs:1320
      >>> THREAD HOP BACK: rendezvous mpsc::sync_channel(0)  text/src/ingest.rs:1461
    normalize_batch                          text/src/ingest.rs:792 -> :1206
  TextStore::dense_hits                      text/src/ingest.rs:802
    TextStore::search_options(tier)          text/src/ingest.rs:895
      tier None  -> SearchOptions::default()  (tier field stays None)
      tier Some  -> SearchOptions::with_tier
    Store::search                            lifecycle/mod.rs:3017
      Store::search_with_graph_bound_mode(GraphBoundMode::Shared)  lifecycle/mod.rs:3647
        QueryControl::with_clock             lifecycle/mod.rs:3655
        Store::admit_vector_search           lifecycle/mod.rs:3742
          state lock / active lock / snapshot RwLock read     :3746 :3755 :3765
          needs_query_pool decision          lifecycle/mod.rs:3776
            Auto  -> true iff any sealed segment LACKS RegionKind 7
            Exact -> false, Scan -> true, Graph -> false
          Store::ensure_query_pool           lifecycle/mod.rs:3694  (lazy, once)
        search_pinned                        lifecycle/mod.rs:4387   <-- see §4
    -> Vec<SearchCandidate>
    per candidate: candidate.document(), score negation        text/src/ingest.rs:820-826
    per candidate: TextStore::make_hit       text/src/ingest.rs:902  <-- PER RESULT
      Store::stored_text                     lifecycle/mod.rs:3149   <-- see §8
```

Volume: `k = 10` candidates in, 10 `make_hit` calls, 10 `Store::stored_text`
calls, each of which re-runs `admit_lexical_query` (3 lock acquisitions).

---

## 4. `search_pinned` — the vector engine (shared by chains 6, 8, 13, 16, 22, 23)

`search_pinned`, `crates/zeppelin-embed/src/lifecycle/mod.rs:4387`.

### 4.1 Once per call, before any segment

```
query finiteness / dimension validation        lifecycle/mod.rs:4412-4430   (768 f32 reads)
OnceCell bit4_query / int8_query allocated     lifecycle/mod.rs:4431-4432   (lazy)
auto_graph_options                             lifecycle/mod.rs:4441
  only when tier() == Auto AND some segment has RegionKind::GraphNodeBlocks
  auto_graph_search_options                    lifecycle/mod.rs:1583
    PublishedSnapshot::graph_profile()  -> AngularClass for this epoch
Graph-tier availability precheck               lifecycle/mod.rs:4453  (explicit Graph only)
full_precision                                 lifecycle/mod.rs:4466
  = tier is Exact | Graph
  OR (tier is Auto AND auto_uses_full_precision(snapshot, active))
     auto_uses_full_precision                  lifecycle/mod.rs:5509
       returns TRUE immediately if any segment has RegionKind 7   :5516-5522
       otherwise TRUE only if the snapshot mixes scheme 0 with scheme 2|4
```

### 4.2 Active segment branch (skipped on both benchmark stores — active is empty)

```
if !active.is_empty()                          lifecycle/mod.rs:4470
  ActiveSegment::alive()                       ingest/active.rs:1507  (rebuilds AliveSet per query)
  Auto && full_precision  -> scan_active_squared_l2   lifecycle/mod.rs:4477
  Auto | Scan             -> prepare_bit4_query + execute_store_scan  :4489
  Exact | Graph           -> scan_active_squared_l2   lifecycle/mod.rs:4520
  fold_worst_squared_l2                        lifecycle/mod.rs:4536 -> :4867
  merge_store_outcome                          lifecycle/mod.rs:4537 -> :5869
  SegmentPlan::unfiltered_scan(ActiveScan)     lifecycle/mod.rs:4551
  retain_global_top_k (Shared bound only)      lifecycle/mod.rs:4558 -> :5616
```

### 4.3 Sealed segment ordering

```
ordered_segments = snapshot.segments()         lifecycle/mod.rs:4562
sort by descending row_count, then ascending id (Shared mode only)  :4567
  manifest metadata only, no artifact I/O
```

One segment in this store, so the sort is a no-op here.

### 4.4 Per sealed segment — tier dispatch

```
for segment in ordered_segments                lifecycle/mod.rs:4576
  SegmentReader::query_alive                   segment/reader.rs:1306
    RegionKind::Alive (2), VERIFYING region() then Arc-cached per reader
  graph_options match options.tier()           lifecycle/mod.rs:4583
    SearchTier::Graph(opts)  -> Some(opts)
    SearchTier::Auto AND directory has RegionKind 7 -> auto_graph_options
    SearchTier::Auto (no region 7) | Exact | Scan   -> None
```

#### 4.4.a Graph branch — `Some(graph_options)`

```
worst_exhaustive = false                       lifecycle/mod.rs:4595
global_competitive_distance(candidates, k)     lifecycle/mod.rs:4596 -> :5621
traverse_segment_graph                         lifecycle/mod.rs:5013
  SnapshotLease::new_at + QueryCancellation     :5031-5032
  GraphSearchCache::prepare_shared              lifecycle/graph_cache.rs:73
    FIRST QUERY ONLY: SegmentReader::graph_node_blocks   segment/reader.rs:1723
      -> region(RegionKind::GraphNodeBlocks) -- VERIFYING, xxh3 over 45.3 MB
    LATER QUERIES: bind_validated_graph_node_blocks      segment/reader.rs:1742
      -> descriptor rebind, no hashing, direct mmap slice
    FIRST QUERY ONLY: GraphSearcher::discover_entry_row_ids  graph/search.rs:1150
    FIRST QUERY ONLY: GraphSegmentNormRange::from_graph      graph_cache.rs:102
    all three cached in CacheState behind one Mutex
  query_rescore_rows_for_search                 lifecycle/mod.rs:5581
    query_rescore_rows                          lifecycle/mod.rs:5573
      SegmentReader::query_rescore_f32          segment/reader.rs:1104
        validate_rescore_byte_range(0, 32)      segment/reader.rs:1848
          EARLY RETURN unless ZE_VERIFY_QUERY_CHECKSUMS is set   :1861
        vector_payload_unchecked                segment/reader.rs:1818
        cast_slice::<f32>                       -> &[f32] BORROWED FROM MMAP, no copy
        RegionKind::VectorRescore (5), 181,186,560 B mapped, not read yet
  segment_k / target_live_k / maximum_candidate_k   lifecycle/mod.rs:5065-5084
  norm-range prune test vs competitive_distance     lifecycle/mod.rs:5124
    -> SegmentGraphResult::Pruned, no traversal        :5136
  GraphSearchCache::checkout(graph, scratch_ef, ...)  graph_cache.rs:115
    reuses a pooled GraphSearchScratch if it supports (node_count, degree, ef)
    else allocates and accounts one                     graph_cache.rs:185-201
  GraphSearcher::with_entry_row_ids                    graph/search.rs:1168
    .with_rescore_validator(segment)                   lifecycle/mod.rs:5155
  loop { GraphSearcher::search }                       lifecycle/mod.rs:5178
    GraphSearcher::search_inner                        graph/search.rs:1315
      pad query to layout().padded_dims()              graph/search.rs:1326  (ALLOC per query)
      prepare_bit4_query(padded, seed)                 graph/search.rs:1334  (ALLOC per query)
      squared_norm(query)                              graph/search.rs:1338
      next_epoch() / scratch clear+reserve              graph/search.rs:1343,1358-1361
      prefetch_line0 per entry seed                    graph/search.rs:1366
      score_and_push_group  <-- PER NODE GROUP OF 4     graph/search.rs:1642
        reads RegionKind 7 node blocks straight from the mmap:
        384 B Bit4 codes + 12 B factors + 4 B flags/degree + 4*degree neighbours
        4,223 candidates scored on the measured query
      mark_visited / heaps                             graph/search.rs:1719,1858-1977
    GraphSearcher::finalize_traversal                  graph/search.rs:1471
      rescore_validator.validate_rows(pool)  <-- PER RETAINED ROW  segment/reader.rs:346
        SegmentReader::validate_rescore_rows           segment/reader.rs:1119
          validate_rescore_byte_range per row          segment/reader.rs:1848
          EARLY RETURN unless ZE_VERIFY_QUERY_CHECKSUMS  (204 no-op calls)
      RescorePool::retained + rescore_top_k            graph/search.rs:1515,1523
        reads 204 * 768 * 4 = 626,688 B of RegionKind 5 f32 rows from the mmap
    tombstone widening loop                            graph/search.rs:5199-5230
      re-runs the WHOLE traversal at candidate_k*2 when live retained < target
      no tombstones on this store, so it breaks on the first iteration  :5199
  SegmentGraphResult::Traversed
SegmentGraphResult::merge_into                         lifecycle/mod.rs:4915
  per candidate: alive.is_alive(row)                   lifecycle/mod.rs:4970,4978
  per candidate: segment.document_version(row)         lifecycle/mod.rs:4993
    SegmentReader::document_version                    segment/reader.rs:1388
      document_versions_region()                       segment/reader.rs:1351
        OnceLock: hashes RegionKind 12 (1,415,520 B) ONCE PER READER
        thereafter region_slice_unaccounted -> direct mmap slice
      24-byte little-endian decode from the mapping
retain_global_top_k                                    lifecycle/mod.rs:4635
```

**[AMPLIFY]** 4,223 nodes scored to return 10. Node-block bytes touched:
`4,223 * 768 = 3,243,264 B` of RegionKind 7, plus `204 * 3,072 = 626,688 B` of
RegionKind 5.

#### 4.4.b Scan / Exact branch — `None`

```
scan_sealed_segment                            lifecycle/mod.rs:5297
  if full_precision                            lifecycle/mod.rs:5318
    exact_rescore_rows_for_search              lifecycle/mod.rs:5552
      exact_rescore_rows                       lifecycle/mod.rs:5544
        SegmentReader::query_rescore_f32       segment/reader.rs:1104   <-- validate-once twin
        RegionKind::VectorRescore (5), &[f32] borrowed from the mmap, NO COPY
    scan_squared_l2                            lifecycle/mod.rs:5693
      build row_indices: Vec<u32> of EVERY alive row      :5718-5729
        58,980 entries = 235,920 B allocated per query
        cancellation checked every 64 rows                :5720
      coarse_scores: vec![0.0f32; 58,980]                 :5744  (235,920 B per query)
      RescorePool::retained(..).with_prefetch(true)       :5745
      quant::rescore_top_k_with_check                     :5753
        reads ALL 181,186,560 B of RegionKind 5
      full sort of 58,980 candidates                      :5790
      worst_score = last element (hybrid anchor)          :5799
      truncate_to_k_with_score_ties                       :5800
    -> single-threaded, on the calling thread
  else match segment.meta().scheme                 lifecycle/mod.rs:5347
    0 -> ScanQuery::F32   + ScanRows::F32BorrowedRowMajor
         SegmentReader::f32_codes                  segment/reader.rs:851 (VERIFYING)
    4 -> ScanQuery::Bit4  + ScanRows::Bit4RowMajor
         SegmentReader::bit4_codes                 segment/reader.rs:811 (VERIFYING)
         SegmentReader::bit4_factors               segment/reader.rs:917 (VERIFYING)
         prepare_bit4_query cached in the OnceCell   lifecycle/mod.rs:5376
    2 -> ScanQuery::Int8  + ScanRows::Int8RowMajor
         SegmentReader::query_int8_factors         segment/reader.rs:1048 (Arc-cached)
         SegmentReader::query_int8_codes           segment/reader.rs:1025 (validate-once twin)
    execute_store_scan                             lifecycle/mod.rs:5446
      QueryPool::execute                           lifecycle/pool.rs:130
        scan_geometry, partition_row_count           :138,145
        >>> THREAD HOP: one WorkItem per pool worker  :159-169
        >>> workers are the persistent store query pool
        execution.wait()                             :172
        merge_partitions(partitions, geometry, workers, k)   :173
fold_worst_squared_l2                            lifecycle/mod.rs:4659 -> :4867
merge_store_outcome                              lifecycle/mod.rs:4660 -> :5869
  per candidate: segment.document_version(row)   lifecycle/mod.rs:4664
SegmentTier decision from directory              lifecycle/mod.rs:4679
  RegionKind 7 present -> SealedGraph, absent -> SealedScan
sealed_scan_reason                               lifecycle/mod.rs:4692 -> :4769
retain_global_top_k                              lifecycle/mod.rs:4695
```

**Note the accessor asymmetry.** The `full_precision` path uses the
validate-once twin `query_rescore_f32`. The Bit4/F32/Int8 *coarse* scan path at
`:5348`, `:5371`, `:5383`, `:5386` still calls the **verifying**
`f32_codes` / `bit4_codes` / `bit4_factors`, each of which routes through
`vector_payload` -> `region()` (`segment/reader.rs:723`) and re-hashes the
whole region with xxh3 **on every query, unconditionally** — `region()` does
not consult `query_checksums_enabled()`. For the scan store that is
22,648,320 B (codes) + 707,760 B (factors) hashed per dense query, on top of
the scan itself.

### 4.5 Once per call, after every segment

```
retain_global_top_k                              lifecycle/mod.rs:4699
ScanStats assembly                               lifecycle/mod.rs:4700
Accounting::record_plans                         lifecycle/mod.rs:4706
QueryDiagnostics::vector                         lifecycle/mod.rs:4707
  exact_rescore: candidates.iter().all(exact_score)   :4713
exact_vector_ceiling                             lifecycle/mod.rs:4729 -> :4807   <-- SEE §9.2
  active branch: GraphSegmentNormRange::from_factors(active.factors())   :4816
  per sealed segment, on meta().scheme:
    4     -> SegmentReader::bit4_factors()      segment/reader.rs:917   <-- VERIFYING
             xxh3 over 707,760 B EVERY QUERY
             GraphSegmentNormRange::from_factors  graph/search.rs:806
               O(rows): 58,980 iterations EVERY QUERY
    0 | 2 -> SegmentReader::rescore_f32()       segment/reader.rs:1090  <-- VERIFYING
             xxh3 over 181,186,560 B EVERY QUERY
             GraphSegmentNormRange::from_exact_rows  graph/search.rs:826
               O(rows * dims): 45,296,640 f64 ops EVERY QUERY
    other -> typed Geometry error
  fold_vector_ceiling                            lifecycle/mod.rs:4848
```

**[AMPLIFY]** `exact_vector_ceiling` runs on *every* tier, including the graph
tier that touches only 4,223 of 58,980 nodes, and it walks all 58,980 factor
records to produce one `f64`. On a scheme 0 or 2 store it would walk the entire
181 MB rescore region per query.

---

## 5. Chain 7 — `query_text(Legs::Lexical)`

```
TextStore::query_text                        text/src/ingest.rs:763
  term_query closure                         text/src/ingest.rs:767
    Analyzer::analyze(text)                  fts/tokenizer/mod.rs:398    (ALLOC Vec<Token>)
    token.term.into_bytes() per token        text/src/ingest.rs:772      (ALLOC per term)
    TermQuery::flat(terms, &[DEFAULT_FIELD]) text/src/ingest.rs:774
  match -> Legs::Lexical                     text/src/ingest.rs:777
  TextStore::lexical_hits                    text/src/ingest.rs:831
    Store::search_lexical                    lifecycle/mod.rs:3079
      NOTE: signature takes NO SearchOptions and NO tier              :3079-3084
      Store::admit_lexical_query             lifecycle/mod.rs:3033
        state lock, active lock, snapshot RwLock read
      SnapshotLease + QueryCancellation      lifecycle/mod.rs:3094-3095
      assemble_lexical_index(.., require_document_identity=true, cancellation=None)
                                             lifecycle/mod.rs:3101 -> :3838   <-- SEE §9.1
      allow_lists: Vec<&DocBitmap> from alive_sets                     :3103
      planner::search_lexical_filtered_refs  planner/lexical.rs:104
        validate_allow_lists                 planner/lexical.rs:141
          per segment: allow_list.iter().find(row >= row_count)        :152
          O(live rows) = 58,980 iterations PER QUERY
        branch selection                     planner/lexical.rs:118
          allowed * 64 <= corpus  -> AllowListDrive
          else                    -> PostCheck
          58,980 * 64 > 58,980, so this store takes PostCheck
        PostCheck -> fts::prune::search_pruned_filtered  planner/lexical.rs:129
          select_strategy(query.terms.len(), k)
          this is the WAND/block-max path: blocks_decoded 818, blocks_skipped 0
      per hit: structured_lexical_document   lifecycle/mod.rs:3118 -> :3991
        Sealed -> SegmentReader::document_version(row)   segment/reader.rs:1388
        Active -> ActiveSegment::document(row)           ingest/active.rs:1489
      LexicalDiagnostics                     lifecycle/mod.rs:3126
    per candidate: TextStore::make_hit       text/src/ingest.rs:840 -> :902  <-- PER RESULT
      Store::stored_text                     lifecycle/mod.rs:3149            <-- see §8
```

**Volume at k=10:** 818 blocks decoded, 7,005 postings decoded, 1,427 documents
evaluated, 10 returned. **[AMPLIFY]** 143 documents evaluated per document
returned; the decode volume is independent of k.

---

## 6. Chain 8 — `query_text(Legs::Hybrid)` — the default

```
TextStore::query_text                        text/src/ingest.rs:763
  term_query closure                         text/src/ingest.rs:767      (as §5)
  TextStore::query_vector                    text/src/ingest.rs:783      (as §3, ANE hop)
  TextStore::hybrid_hits                     text/src/ingest.rs:850
    HybridQuery::new(k)                      fusion/mod.rs:164
      .with_alpha(bundle.hybrid_alpha())     text/src/ingest.rs:863
      .with_epoch(self.document_epoch)       text/src/ingest.rs:864
      max_rounds defaults to DEFAULT_MAX_ROUNDS = 8   fusion/mod.rs:42,170
    Store::search_hybrid                     lifecycle/mod.rs:3331
      Store::search_hybrid_inner(PinnedLexicalQuery::Term)  lifecycle/mod.rs:3367
```

### 6.1 `search_hybrid_inner` — once per query

```
control.with_clock                           lifecycle/mod.rs:3375
requested_tier = options.explicit_tier()     lifecycle/mod.rs:3378
Store::admit_vector_search                   lifecycle/mod.rs:3394 -> :3742
tier_resolution                              lifecycle/mod.rs:3397
  requested_tier.is_none() AND snapshot_has_graph -> HybridTierResolution::Auto,
                                                     options.tier stays None
  requested_tier.is_none() AND no graph       -> options = options.with_tier(Exact)
                                                     HybridTierResolution::Exact
  requested_tier.is_some()                    -> None, honour the caller
  snapshot_has_graph                          lifecycle/mod.rs:4798
    scans every segment's directory for RegionKind 7
hybrid::corpus_rows                          lifecycle/mod.rs:3407 -> hybrid.rs:48
  active.row_count() + sum(segment.meta().row_count) = 58,980  (manifest only)
initial width                                lifecycle/mod.rs:3412
  max_rounds == 0 -> corpus_rows
  else hybrid::hybrid_window(k, corpus_rows)  hybrid.rs:33
    width = max(HYBRID_WINDOW_PER_K * k, HYBRID_WINDOW_FLOOR, k).min(corpus)
          = max(5*10, 50, 10) = 50
Store::ensure_lexical_worker                 lifecycle/mod.rs:3419 -> :3719
  LexicalWorker::start (lazy, once per store)  lifecycle/pool.rs:306
  >>> spawns named thread "zeppelin-fts"       lifecycle/pool.rs:310
```

### 6.2 Per round — what re-runs

Round 0 is `lifecycle/mod.rs:3458` (lexical) and `:3509` (vector). Rounds 1..n
are `:3578` (lexical) and `:3579` (vector), inside the loop at `:3538`.

```
LOOP BODY (lifecycle/mod.rs:3538) -- runs `rounds` times:
  hybrid::build_round                        lifecycle/mod.rs:3539 -> hybrid.rs:96
  fusion::fuse_bounded                       lifecycle/mod.rs:3549 -> fusion/mod.rs:640
  termination test                           lifecycle/mod.rs:3557
    break unless termination == WindowUnproven AND width < corpus_rows
  width doubling / budget exhaustion         lifecycle/mod.rs:3571-3577
    rounds >= max_rounds -> width = corpus_rows, budget_exhausted = true
    else width = min(width * 2, corpus_rows)
  submit_lexical_leg(width + 1)              lifecycle/mod.rs:3578      RE-RUNS
  run_vector_leg(width + 1)                  lifecycle/mod.rs:3579      RE-RUNS
  resolve_hybrid_leg_results                 lifecycle/mod.rs:3583 -> :4153
```

**Re-runs per round:** the complete lexical leg (index assembly included — see
§9.1), the complete vector leg (`search_pinned`, including
`exact_vector_ceiling`), `build_round` with all its cross-fill, and
`fuse_bounded`.

**Runs once per query:** admission, tier resolution, `corpus_rows`,
`hybrid_window`, `ensure_lexical_worker`, the two `consume_hybrid_test_fault`
calls, and every diagnostics assembly after the loop (`:3591`–`:3628`).

On the measured FiQA query the loop executes **once** (`rounds: 1`,
termination `StableBound`), so the widening machinery costs nothing here beyond
one extra comparison. It is a cliff, not a cost: the second round doubles the
window to 100 and re-runs both legs from scratch, including a fresh
`assemble_lexical_index`.

### 6.3 The two legs, in parallel

```
submit_lexical_leg(bound)                    lifecycle/mod.rs:3420
  LexicalWorker::submit                      lifecycle/pool.rs:347
    >>> THREAD HOP: mpsc::Sender to "zeppelin-fts"   lifecycle/pool.rs:391
    closure body runs on "zeppelin-fts":
      SnapshotLease::new_at + QueryCancellation      lifecycle/mod.rs:3428-3430
      PinnedLexicalQuery::Term -> exact_lexical_leg  lifecycle/mod.rs:3436 -> :4273
        assemble_lexical_index(.., require_document_identity=FALSE,
                               cancellation=Some)    lifecycle/mod.rs:4285   <-- SEE §9.1
        k = bound.min(index.document_count())        lifecycle/mod.rs:4304
        allow_lists                                  lifecycle/mod.rs:4305
        exact_hybrid_lexical_search                  lifecycle/mod.rs:4309 -> :4338
          allowed = sum of allow_list cardinalities   :4345
          allowed * LEXICAL_ALLOW_LIST_DIVISOR(64) <= document_count ?
            YES -> fts::search::search_allow_list_driven_controlled  :4351
                   fts/search.rs:387
            NO  -> fts::prune::search_pruned_filtered                :4366
                   (this store: 58,980 alive, so NO -> pruned path)
        per hit: structured_lexical_document(.., false)  lifecycle/mod.rs:4312
      PinnedLexicalQuery::Structured -> exact_structured_lexical_leg  :3444 -> :4187
        adds fts::query::vocabulary + expand and ONE SEARCH PER EXPANSION  :4226-4238
    >>> THREAD HOP BACK: PendingLexical::wait          lifecycle/pool.rs:270
                                                       called at lifecycle/mod.rs:3517

run_vector_leg(k)                            lifecycle/mod.rs:3460
  runs on the CALLER thread inside catch_unwind          lifecycle/mod.rs:3509
  graph_available = snapshot_has_graph                   lifecycle/mod.rs:3461
  scan_reason_override                                   lifecycle/mod.rs:3462
    caller chose a tier, or no graph -> None
    k > corpus_rows                  -> ScanReason::FullMaterialization
    graph_round_is_worth_it(k, corpus_rows) -> None
    else                             -> ScanReason::WideningCap
  effective options                                      lifecycle/mod.rs:3494
    caller_chose_tier OR graph_round_is_worth_it(k, rows) -> options as-is
    else                                                  -> options.with_tier(Exact)
    graph_round_is_worth_it                               lifecycle/mod.rs:4755
      ef = graph_round_ef(k) = max(4*k, 140)              lifecycle/mod.rs:4765
      worth it iff ef <= corpus_rows / 4 = 14,745
      at k = 51: ef = 204 <= 14,745 -> graph stays
      the graph is abandoned once k > 3,686
  search_pinned                                          lifecycle/mod.rs:3474   <-- §4
```

### 6.4 `build_round` and cross-fill

```
hybrid::build_round                          lifecycle/hybrid.rs:96
  vector_window = vector[..width]            hybrid.rs:106
  lexical_window = lexical[..width]          hybrid.rs:107
  vector_next / lexical_next = element [width]  hybrid.rs:108-109   (the unseen bound)
  vector_keys BTreeSet                       hybrid.rs:111          (ALLOC per round)
  lexical_keys BTreeSet                      hybrid.rs:115          (ALLOC per round)
  vector_candidates Vec                      hybrid.rs:120          (ALLOC per round)
  CROSS-FILL VECTOR  <-- PER LEXICAL-WINDOW DOCUMENT NOT IN THE VECTOR WINDOW
    for hit in lexical_window                hybrid.rs:133
      exact_squared_l2                       hybrid.rs:140 -> hybrid.rs:217
        resolve StructuredLexicalSource      hybrid.rs:229-232
        Sealed -> crate::lifecycle::exact_rescore_rows(segment)   hybrid.rs:239
                  lifecycle/mod.rs:5544
                  SegmentReader::query_rescore_f32   segment/reader.rs:1104
                    <-- VALIDATE-ONCE TWIN, borrowed &[f32], NO COPY
                    <-- this is the accessor commit 2bc1505 fixed;
                        the verifying rescore_f32 here cost 219 ms of 221 ms
        Active -> ActiveSegment::vectors()   hybrid.rs:241
        row slice = rows[row*768 .. row*768+768]   hybrid.rs:244-249
        quant::squared_l2_f64(query, row)    hybrid.rs:250
          reads 3,072 B of RegionKind 5 from the mmap
  CROSS-FILL LEXICAL  <-- PER LEXICAL HIT PAST THE WINDOW, no byte access
    for hit in lexical[width..]              hybrid.rs:150
  two sorts                                  hybrid.rs:161-162
  VectorBounds from vector_ceiling           hybrid.rs:164-188
  LexicalBounds from window first / list last  hybrid.rs:189-196
```

**Measured:** `cross_filled_vector = 50`, so `exact_squared_l2` runs 50 times,
each reading 3,072 B — 153,600 B total off the 181 MB rescore region. Every
call goes through the validate-once twin now; before `2bc1505` each call
re-hashed all 181,186,560 B, which is the 219 ms defect.

**`cross_filled_lexical = 0`** because the lexical leg returned exactly 50 hits
(`lexical_returned = 50`), so `lexical[width..]` is empty. `vector_returned` is
100 = 50 window + 50 cross-filled.

### 6.5 After the loop

```
destructure SearchOutcome                    lifecycle/mod.rs:3591
report.rounds / budget_exhausted             lifecycle/mod.rs:3601-3605
QueryDiagnostics::hybrid                     lifecycle/mod.rs:3606
StoreHybridSearchOutcome                     lifecycle/mod.rs:3623
back in TextStore::hybrid_hits:
  versions Mutex lock                        text/src/ingest.rs:869
  per hit: versions.get(&hit.key)            text/src/ingest.rs:877   <-- PER RESULT
  per hit: TextStore::make_hit               text/src/ingest.rs:881   <-- PER RESULT
    Store::stored_text                       lifecycle/mod.rs:3149    <-- §8
```

---

## 7. Chains 13–17 — the core `Store` seams

### 7.1 `Store::search` (chain 13)

`lifecycle/mod.rs:3017` -> `search_with_graph_bound_mode(.., GraphBoundMode::Shared)`
at `:3024` -> `:3647` -> `admit_vector_search` `:3742` -> `search_pinned` `:4387`.
`GraphBoundMode::Independent` at `:1617` is `#[cfg(test)]` only, reachable
solely through `search_independent_for_test` at `:3808`.

Tier semantics, all three levels:

| caller passed | `SearchOptions.tier` | `options.tier()` (`:1644`) | effect in `search_pinned` |
| --- | --- | --- | --- |
| nothing (`--tier unset`) | `None` | `Auto` | graph if RegionKind 7 present, else Bit4 scan |
| `SearchTier::Auto` | `Some(Auto)` | `Auto` | identical execution, but `explicit_tier()` is `Some` |
| `SearchTier::Exact` | `Some(Exact)` | `Exact` | `full_precision`, f32 scan of every alive row |
| `SearchTier::Scan` | `Some(Scan)` | `Scan` | pool scan on the segment's own scheme |
| `SearchTier::Graph(o)` | `Some(Graph(o))` | `Graph(o)` | hard error if any segment lacks RegionKind 7 (`:4460`) |

`explicit_tier()` (`:1651`) is what separates unset from explicit `Auto`; it
feeds `sealed_scan_reason` (`:4769`) and hybrid's tier resolution (`:3397`).

### 7.2 `Store::search_lexical` (chain 14)

Detailed in §5. Note `require_document_identity = true` at `:3101` and
`cancellation = None` at `:3101` — the standalone path does *not* pass a
cancellation handle into index assembly, unlike both hybrid legs.

### 7.3 `Store::search_lexical_structured` (chain 15)

```
Store::search_lexical_structured             lifecycle/mod.rs:3190
  admit_lexical_query                        lifecycle/mod.rs:3200 -> :3033
  assemble_lexical_index(.., true, Some(&cancellation))   lifecycle/mod.rs:3214
  fts::query::vocabulary(query, index.terms())            lifecycle/mod.rs:3222
    index.terms() iterates EVERY term of EVERY segment    fts/index.rs:655
  fts::query::expand(query, &vocabulary)                  lifecycle/mod.rs:3223
  PER EXPANSION (lifecycle/mod.rs:3235):
    build a fresh single-term TermQuery                   :3237  (ALLOC per expansion)
    search_allow_list_driven_controlled(all_rows)         :3241
      note k = all_rows, not the caller's k               :3234
    accumulate into a BTreeMap                            :3253
  optional phrase filter, PER AGGREGATED DOC              :3260
    structured_lexical_row -> stored_text bytes           :3261 -> :4097
  sort + truncate to k                                    :3268-3275
  PER RETURNED CANDIDATE (:3277):
    structured_lexical_row                                :3279 -> :4097
      Sealed -> SegmentReader::stored_text()   segment/reader.rs:1583  <-- VERIFYING
                walks EVERY row offset and re-hashes RegionKind 14 per call
    fts::snippet::best_window                             :3284
```

**[AMPLIFY]** This path calls the *verifying* `stored_text()` (not
`query_stored_text`) once per returned candidate, and again once per aggregated
document when a phrase constraint is present. Each call re-hashes the whole
`StoredText` region and walks all 58,980 row offsets. It is not on the
benchmark's hot path (`query_text` never reaches it) but it is the same class of
defect as the one fixed in `2bc1505`.

### 7.4 `Store::search_hybrid` / `search_hybrid_structured` (chains 16, 17)

`:3331` and `:3350` both funnel into `search_hybrid_inner` at `:3367`; the only
difference is `PinnedLexicalQuery::Term` vs `::Structured` (`:3341`, `:3360`),
which selects `exact_lexical_leg` (`:4273`) or `exact_structured_lexical_leg`
(`:4187`) inside the worker closure at `:3435`.

---

## 8. Chain 18 — `Store::stored_text`

```
Store::stored_text                           lifecycle/mod.rs:3149
  Store::admit_lexical_query                 lifecycle/mod.rs:3153 -> :3033
    3 lock acquisitions PER CALL, and this is called ONCE PER RETURNED HIT
  ActiveSegment::existing(doc_id)            lifecycle/mod.rs:3159 -> ingest/active.rs:1497
    LINEAR SCAN of doc_ids                   ingest/active.rs:1498-1501
    O(active rows); zero on these stores because the active segment is empty
  ActiveSegment::text(row)                   lifecycle/mod.rs:3162 -> ingest/active.rs:1372
  for segment in snapshot.segments()         lifecycle/mod.rs:3168
    SegmentReader::query_row_for_document_version   segment/reader.rs:1438
      document_versions_region()             segment/reader.rs:1351
        OnceLock, hashes 1,415,520 B of RegionKind 12 once per reader
      query_document_version_index           segment/reader.rs:1461
        FIRST CALL PER READER: sorts 58,980 u32 by 24-byte key      :1468-1473
        Arc-cached and accounted thereafter                         :1484-1490
      partition_point binary search                                 :1452
    SegmentReader::query_stored_text          lifecycle/mod.rs:3176 -> segment/reader.rs:1642
      VALIDATE-ONCE TWIN. With ZE_VERIFY_QUERY_CHECKSUMS unset it takes the
      early branch at :1643 -> region_slice + stored_text_view, header geometry
      only, direct mmap slice, no row walk, no hash.
      First call with verification on: falls through to the verifying
      stored_text() at :1654, then latches stored_text_validated.
    rows.row(row)                             lifecycle/mod.rs:3179
    str::to_owned                             lifecycle/mod.rs:3180   <-- COPIES the text
```

**Per result, not per query.** At k=10 that is 10 admissions, 10 binary
searches, 10 `String` allocations. `RegionKind::StoredText` (14) is the region;
the accessor is the validate-once twin; the mmap is read directly and only the
final `String` is copied.

---

## 9. The two questions about shared work

### 9.1 `assemble_lexical_index` — both call sites

`lifecycle/mod.rs:3838`. Four call sites, three of them on query paths:

| site | file:line | `require_document_identity` | `cancellation` | runs on |
| --- | --- | --- | --- | --- |
| `Store::search_lexical` | `:3101` | `true` | `None` | caller thread |
| `Store::search_lexical_structured` | `:3214` | `true` | `Some` | caller thread |
| `exact_structured_lexical_leg` (hybrid) | `:4205` | `false` | `Some` | `zeppelin-fts` |
| `exact_lexical_leg` (hybrid) | `:4285` | `false` | `Some` | `zeppelin-fts` |

**The work is repeated, never shared.** Nothing is cached across calls, across
rounds, or between the standalone and hybrid paths. Each call builds a fresh
`LexicalIndex` (`:3845`) and, per segment (`:3848`):

```
SegmentReader::query_postings                segment/reader.rs:1221
  Arc-cached per reader after the first decode; the underlying region()
  call at :1235 hashes RegionKind 6 once, then the Arc is reused
if require_document_identity: segment.document_version(0)   lifecycle/mod.rs:3861
SegmentReader::query_alive                   lifecycle/mod.rs:3870 -> reader.rs:1306
  Arc-cached per reader
LexicalIndex::push_shared_with_live_rows     lifecycle/mod.rs:3871 -> fts/index.rs:632
  live_segment_counters                      fts/index.rs:717
    live_rows.iter().find(row >= row_count)  fts/index.rs:722   O(58,980) PER CALL
    live_rows.iter().try_fold(sum lengths)   fts/index.rs:729   O(58,980) PER CALL
  self.live_rows.push(live_rows.clone())     fts/index.rs:641   ROARING CLONE PER CALL
then for the active segment (lifecycle/mod.rs:3878):
  ActiveSegment::sealed_lexical(accounting)  ingest/active.rs:1414  OnceCell-cached
  ActiveSegment::alive()                     ingest/active.rs:1507  REBUILT PER CALL
```

So one `assemble_lexical_index` call is **two full 58,980-element bitmap walks
plus one Roaring bitmap clone**, per sealed segment, with the postings and alive
sets themselves Arc-cached. The hybrid path pays this once per round; a
two-round hybrid pays it twice. The standalone lexical path pays it once.

`planner::search_lexical_filtered_refs` then immediately walks the same bitmap a
third time in `validate_allow_lists` (`planner/lexical.rs:152`).

The **arguments differ** in exactly one behavioural way: hybrid passes
`require_document_identity = false`, so a postings-bearing segment without a
`DocumentVersions` region is tolerated by hybrid and rejected by
`Store::search_lexical`. Neither difference changes the volume of work.

### 9.2 Why lexical does not care about the tier

`TextStore::query_text` reads `options.tier` only inside the `Legs::Dense`
(`:780`) and `Legs::Hybrid` (`:784`) arms. The `Legs::Lexical` arm at `:777`
calls `lexical_hits(&term_query(), options.k)` — `tier` is never read. Below
that, `Store::search_lexical` (`lifecycle/mod.rs:3079`) takes no `SearchOptions`
parameter at all: its signature is `(query, k, control)`. Nothing on the lexical
path consults `SearchTier`, `RegionKind::GraphNodeBlocks`, or
`auto_uses_full_precision`. The only structural difference between the two
stores is the presence of RegionKind 7, which the lexical path never inspects.
1.414 vs 1.389 ms is measurement noise on identical work. **Confirmed from
code.**

---

## 10. Why the tiers differ

### 10.1 Dense: 1.055 ms (graph) vs 4.542 ms (scan), 4.3x

With `--tier unset`, `options.tier()` resolves to `Auto` at
`lifecycle/mod.rs:1644` in both cases. The divergence is one branch:

| hop | graph store | scan store |
| --- | --- | --- |
| `auto_uses_full_precision` `:5509` | `true` (early return at `:5521`, RegionKind 7 present) | `false` (no scheme 0 segment, so `has_exact` stays false) |
| `full_precision` `:4466` | `true` | `false` |
| `graph_options` `:4583` | `auto_graph_options` = `Some(Angular)` | `None` |
| branch taken | `traverse_segment_graph` `:4603` | `scan_sealed_segment` `:4640` |
| rows touched | 4,223 scored + 204 rescored | **58,980**, all of them |
| bytes touched | 3,243,264 (RegionKind 7) + 626,688 (RegionKind 5) | 22,648,320 (codes) + 707,760 (factors), RegionKinds 3 and 4 |
| threads | 1 (caller) | full query pool, `pool.rs:159` |
| accessor | `bind_validated_graph_node_blocks` (no hash) | `bit4_codes` + `bit4_factors`, **both verifying**, `reader.rs:811`/`:917` |

The scan store additionally re-hashes 23,356,080 B per query in those two
verifying accessors before the scan even starts, on top of the 58,980-row
scan itself. That is a specific, isolatable difference in this ratio.

### 10.2 Hybrid: 2.899 ms (graph) vs 32.327 ms (scan), 11.1x

The scan store has no graph, so `search_hybrid_inner` rewrites the tier:

```
requested_tier = None                            lifecycle/mod.rs:3378
snapshot_has_graph(&admitted.snapshot) == false  lifecycle/mod.rs:3398
options = options.with_tier(SearchTier::Exact)   lifecycle/mod.rs:3401
tier_resolution = HybridTierResolution::Exact    lifecycle/mod.rs:3402
```

Hybrid may only fuse exactly-rescored scores, and a Bit4 scan produces
estimates, so it forces `Exact` rather than accepting the Bit4 tier that the
dense leg was happy with. `search_pinned` then sets `full_precision = true`
(`:4466`) and `scan_sealed_segment` takes the `full_precision` branch at `:5318`:

| hop | graph store hybrid | scan store hybrid |
| --- | --- | --- |
| effective tier | `Auto` -> graph traversal | **`Exact`** |
| vector leg | `traverse_segment_graph` `:4603` | `scan_squared_l2` `:5693` |
| rows scored | 4,223 | **58,980** |
| rescore bytes read | 626,688 | **181,186,560** (the whole RegionKind 5) |
| per-query allocations | pooled scratch, reused | `row_indices` 235,920 B + `coarse_scores` 235,920 B, fresh |
| sort | 204-element heap | full sort of 58,980 candidates `:5790` |
| threads | 1 | 1 — `scan_squared_l2` is single-threaded |
| lexical leg | identical | identical |
| cross-fill | 50 x 3,072 B | 50 x 3,072 B, identical |

So the 11x is the vector leg alone: a 181 MB single-threaded exact scan against
a 3.2 MB graph traversal. Note the scan-tier *dense* leg is 7x faster than the
scan-tier *hybrid* leg (4.542 vs 32.327 ms) on the same store, purely because
dense accepts Bit4 estimates and hybrid does not.

---

## 11. Byte-touch summary

Every place a query path first reaches mmap bytes.

| accessor | `RegionKind` | verifying or twin | mmap direct or copy | reached from |
| --- | --- | --- | --- | --- |
| `query_alive` `reader.rs:1306` | Alive (2) | verifying `region()` once, then Arc-cached | decoded into an Arc'd `AliveSet` | `search_pinned:4577`, `assemble_lexical_index:3870` |
| `query_postings` `reader.rs:1221` | Postings (6) | verifying `region()` once, then Arc-cached | decoded into an Arc'd `SealedSegment` | `assemble_lexical_index:3854` |
| `graph_node_blocks` `reader.rs:1723` | GraphNodeBlocks (7) | **verifying**, first query only | direct slice | `graph_cache.rs:91` |
| `bind_validated_graph_node_blocks` `reader.rs:1742` | GraphNodeBlocks (7) | descriptor rebind, no hash | direct slice | `graph_cache.rs:86` |
| `query_rescore_f32` `reader.rs:1104` | VectorRescore (5) | **validate-once twin** | `&[f32]` cast from the mapping, no copy | `query_rescore_rows:5573`, `exact_rescore_rows:5544`, `hybrid.rs:239` |
| `rescore_f32` `reader.rs:1090` | VectorRescore (5) | **verifying, hashes 181 MB every call** | `&[f32]`, no copy | `exact_vector_ceiling:4830` (scheme 0/2 only) |
| `bit4_codes` `reader.rs:811` | VectorCodes (3) | **verifying, hashes 22.6 MB every call** | `&[u8]`, no copy | `scan_sealed_segment:5383` |
| `bit4_factors` `reader.rs:917` | VectorFactors (4) | **verifying, hashes 708 KB every call** | `&[Bit4Factors]`, no copy | `scan_sealed_segment:5386`, `exact_vector_ceiling:4824` |
| `f32_codes` `reader.rs:851` | VectorCodes (3) | **verifying** | `&[f32]`, no copy | `scan_sealed_segment:5353` |
| `query_bit4_codes` `reader.rs:1019` | VectorCodes (3) | validate-once twin | `&[u8]`, no copy | **GAP: no query-path caller found** |
| `query_bit4_factors` `reader.rs:1040` | VectorFactors (4) | validate-once twin | no copy | **GAP: no query-path caller found** |
| `query_int8_codes` `reader.rs:1025` | VectorCodes (3) | validate-once twin | `&[i8]`, no copy | `scan_sealed_segment:5418` |
| `query_int8_factors` `reader.rs:1048` | VectorFactors (4) | verifying `int8_factors()` once, then Arc-cached `Vec` | **copies** into an accounted `Vec<Int8Factors>` | `scan_sealed_segment:5405` |
| `document_version` `reader.rs:1388` | DocumentVersions (12) | `OnceLock`-validated region, then unaccounted slice | 24-byte decode from the mapping | `merge_into:4993`, `merge_store_outcome:5908`, `structured_lexical_document:4010`, `assemble_lexical_index:3861` |
| `query_row_for_document_version` `reader.rs:1438` | DocumentVersions (12) | same `OnceLock`, plus an Arc-cached sorted index | binary search over the mapping | `Store::stored_text:3170` |
| `query_stored_text` `reader.rs:1642` | StoredText (14) | validate-once twin | header-geometry view over the mapping | `Store::stored_text:3176` |
| `stored_text` `reader.rs:1583` | StoredText (14) | **verifying, plus a full row-offset walk every call** | view over the mapping | `structured_lexical_row:4116` |
| `region(ChecksumTable)` `reader.rs:1935` | ChecksumTable (11) | verifying, and re-hashed on **every** `chunk_checksum` call | linear scan of the table | `region_chunk:787`, only when `ZE_VERIFY_QUERY_CHECKSUMS` is set |

`query_checksums_enabled` (`reader.rs:989`) gates only the validate-once twins
and `validate_rescore_byte_range`. It does **not** gate `region()`
(`reader.rs:723`), which always hashes. Every "verifying" row above therefore
hashes on every call regardless of the environment variable.

---

## 12. Per-candidate and per-row work

Where the last three defects lived. `N` = candidates, `R` = rows, `k` = results.

| site | file:line | frequency | cost |
| --- | --- | --- | --- |
| `exact_squared_l2` | `hybrid.rs:217` | per cross-filled doc (50 measured) | `query_rescore_f32` + 3,072 B read. **Was 219 ms of 221 ms via the verifying twin before `2bc1505`.** |
| `exact_vector_ceiling` | `mod.rs:4807` | once per `search_pinned`, i.e. per hybrid round | verifying `bit4_factors` (708 KB hash) + `from_factors` O(58,980) |
| `live_segment_counters` | `fts/index.rs:717` | per `assemble_lexical_index`, per segment | two O(58,980) bitmap walks |
| `DocBitmap::clone` | `fts/index.rs:641` | per `assemble_lexical_index`, per segment | full Roaring clone |
| `validate_allow_lists` | `planner/lexical.rs:152` | per `search_lexical_filtered_refs`, per segment | O(58,980) bitmap walk |
| `merge_store_outcome` document lookup | `mod.rs:5908` | per scan candidate (k + ties) | 24-byte mmap decode |
| `merge_into` document lookup | `mod.rs:4993` | per graph candidate (204) | 24-byte mmap decode |
| `validate_rescore_rows` | `reader.rs:1119` | per retained graph row (204) | no-op unless verification is on |
| `score_and_push_group` | `graph/search.rs:1642` | per group of 4 nodes (4,223 nodes) | 768 B node block each |
| `TermStream::open` (RowDriven) | `fts/search.rs:453` | **per (allow-listed row x term)** | only on the `AllowListDrive` branch; this store takes `PostCheck`, so not hit |
| `TermScorer::score` | `fts/search.rs:532` | per decoded posting (7,005 at k=10) | — |
| `TextStore::make_hit` | `text/ingest.rs:902` | per returned hit (10) | full `Store::stored_text` incl. 3 lock acquisitions |
| `ActiveSegment::existing` | `ingest/active.rs:1497` | per `stored_text` call | linear scan of active `doc_ids`; zero here |
| `scan_squared_l2` row-index build | `mod.rs:5719` | per row (58,980), Exact tier only | bitmap test + `Vec` push |
| `structured_lexical_row` | `mod.rs:4097` | per candidate, structured lexical only | **verifying** `stored_text()`: full region hash + row walk |
| `fts::query::expand` search | `mod.rs:3241`, `:4232` | **per expansion term** | one complete index search each |

---

## 13. Thread hops

| hop | from | to | mechanism |
| --- | --- | --- | --- |
| query embed | caller | `ze-text-embed` | `mpsc::sync_channel(2)`, `text/ingest.rs:1367`; spawned `:1370`; reply `sync_channel(0)` `:1461` |
| hybrid lexical leg | caller | `zeppelin-fts` | `LexicalWorker::submit`, `pool.rs:347`; spawned `pool.rs:310`; reply `sync_channel(0)` `pool.rs:371`; awaited `mod.rs:3517`, `:3580` |
| hybrid vector leg | — | caller thread | runs inline inside `catch_unwind`, `mod.rs:3509` |
| scan-tier partitions | caller | query pool workers | `QueryPool::execute`, `pool.rs:130`; one `WorkItem` per worker `:159`; joined at `:172` |
| graph traversal | — | caller thread | serial per segment, `mod.rs:4603`; `caller_thread` recorded at `:5247` |
| exact (`full_precision`) scan | — | caller thread | `scan_squared_l2` is single-threaded, `mod.rs:5817` |
| ingest tokenizers | caller | `ze-text-tok-{0..n}` | `text/ingest.rs:479`, up to 4 workers |
| ingest maintenance | caller | `ze-text-maintain` | `text/ingest.rs:522` |

The query pool is created lazily and only when `needs_query_pool` is true
(`mod.rs:3776`): never for `Exact` or explicit `Graph`, always for `Scan`, and
for `Auto` only when some segment lacks RegionKind 7. On the graph store a
hybrid or dense query allocates **no** pool.

---

## 14. Per-query allocations and index assembly that could plausibly be cached

Ranked by size on this store.

1. `assemble_lexical_index` — the whole `LexicalIndex`, per lexical query and
   per hybrid round (`mod.rs:3838`). The postings and alive sets inside it are
   already Arc-cached per reader; the counters, the Roaring clone, and the
   `Vec` scaffolding are not. Nothing in it depends on the query.
2. `scan_squared_l2` `row_indices` + `coarse_scores` — 471,840 B per Exact-tier
   query (`mod.rs:5718`, `:5744`). Depends only on the alive set.
3. `exact_vector_ceiling` — one `f64` per query derived from an O(rows) walk
   (`mod.rs:4807`). Depends only on the segment factors and the query norm;
   the `GraphSegmentNormRange` half is query-independent and is already cached
   for the graph path in `graph_cache.rs:99`, just not for this caller.
4. `GraphSearcher::search_inner` padded query + `prepare_bit4_query` —
   `graph/search.rs:1329`, `:1334`, per traversal, per round.
5. `build_round`'s two `BTreeSet`s and two `Vec`s — `hybrid.rs:111`, `:115`,
   `:120`, `:145`, per round.
6. `TermQuery` construction in `query_text` — one `Vec<u8>` per analyzed term
   (`text/ingest.rs:772`), plus `TermQuery::flat`'s field vector, rebuilt per
   expansion inside the structured paths (`mod.rs:3237`, `:4227`).
7. `allow_lists: Vec<&DocBitmap>` — rebuilt at `mod.rs:3103`, `:3224`, `:4218`,
   `:4305`.
8. `ordered_segments` `Vec` and its sort — `mod.rs:4562`, per query, one entry
   here.
9. `Store::stored_text`'s `String` per hit — `mod.rs:3180`; unavoidable across
   the API boundary, but the surrounding admission is not.

---

## 15. Gaps

- **`query_bit4_codes` / `query_bit4_factors` have no query-path caller.** The
  validate-once twins exist at `reader.rs:1019` and `:1040`, but
  `scan_sealed_segment` calls the verifying `bit4_codes` / `bit4_factors` at
  `mod.rs:5383` and `:5386`. I could not find any production caller of the two
  Bit4 twins. Either they are dead, or the scan path was meant to use them and
  does not. Stated as a gap, not a conclusion.
- **`--tier unset` vs `--tier auto` in `text-user-bench`.** I traced the
  semantics through `QueryOptions::with_optional_tier` (`text/query.rs:60`) and
  `TextStore::search_options` (`text/ingest.rs:895`), but I did not read the
  bench binary's flag parsing, so I cannot state which `Option<SearchTier>` the
  string `unset` produces. I assumed `None`, which is consistent with the
  measured graph/scan divergence.
- **`fts::prune::search_pruned_filtered` internals.** I confirmed it is the
  branch this store takes (`allowed * 64 > corpus`, `mod.rs:4349`,
  `planner/lexical.rs:119`) and that it is where `blocks_decoded` / `blocks_skipped`
  originate, but I did not map its internal block-max/WAND structure. That is
  the one hop in the lexical chain whose interior is unmapped.
- **`snapshot.graph_profile()` resolution.** I inferred `AngularClass` from
  `ef = 204` at `k = 51` matching `ANGULAR_EF_PER_K = 4` (`graph/search.rs:29`,
  `:319`), not from reading the epoch registry entry for this bundle.
- **`Store::health` and `Store::maintain` interiors** are mapped only to their
  entry points (`diag.rs:1118`, `tier/maintain.rs:199`). They are not on the
  query path; `health()` does call the **verifying** `segment.alive()`
  (`diag.rs`, in the `all_segments()` loop) rather than `query_alive`, which is
  correct for a health check.
- The graph node-block region size derivation in §2.3 leaves 16,064 B
  unexplained; I attribute it to 16-KB region alignment but did not read the
  writer to confirm.

---

## 16. Candidates for instrumentation

Ranked. Each is a boundary, not a function: time the enter/exit pair.

1. **`scan_sealed_segment` full-precision branch, `mod.rs:5318`–`:5341`, split
   into `exact_rescore_rows_for_search` vs `scan_squared_l2`.**
   This is the entire hybrid scan-tier gap: 181 MB, single-threaded, 58,980
   rows to return 10. Splitting the accessor from the scan says whether the
   cost is page-fault-bound on first touch or arithmetic-bound. It is the
   largest single number in the whole matrix.

2. **`exact_vector_ceiling`, `mod.rs:4729` call site.**
   Runs on every tier, every round, unconditionally, and does O(corpus) work
   plus a verifying region hash to produce one scalar — on the graph path that
   otherwise touches 7% of the segment. If this is 100 µs of a 1.055 ms dense
   query it is 10% of the graph tier's entire budget, and it is cacheable. This
   is the same *shape* as the three defects already found: a hot path calling a
   verifying accessor.

3. **`assemble_lexical_index`, `mod.rs:3838`, timed separately from the search
   that follows it.**
   Two 58,980-element bitmap walks plus a Roaring clone per call, once per
   lexical query and once per hybrid round, on the `zeppelin-fts` thread. It is
   pure setup and entirely query-independent. Timing it splits the 1.4 ms
   lexical figure into "assembly" and "actual retrieval", which is the single
   most useful decomposition in the lexical leg because it tells you whether
   `blocks_decoded = 818` is the cost or a red herring.

Then, in order:

4. `bit4_codes` + `bit4_factors` at `mod.rs:5383`/`:5386` — the 23.4 MB of
   unconditional xxh3 on every scan-tier dense query, isolated from the scan.
5. `RuntimeClient::embed` round trip, `text/ingest.rs:1449`–`:1485` — the ANE
   hop; it is the one leg shared by dense and hybrid and is invisible in the
   store's own diagnostics.
6. `search_pinned` `:4387` vs the `zeppelin-fts` leg `:3458` — measure the two
   hybrid legs' overlap directly, to confirm the parallelism is real and find
   the straggler.
7. `build_round` `hybrid.rs:96`, split into cross-fill (`:133`) and the two
   sorts (`:161`) — the cross-fill is 50 `exact_squared_l2` calls; this is the
   exact site of the 219 ms defect and deserves a permanent counter.
8. `Store::stored_text` `:3149`, aggregated over the k calls per query — per
   result, three locks each, plus a `String` copy.
9. `QueryPool::execute` `pool.rs:130`, dispatch-to-join minus the partition
   work — the pool's own overhead on a 4.5 ms query.
10. `graph_cache::prepare_shared` `graph_cache.rs:73` — should be near-zero
    after the first query; a non-zero steady-state value means the cache is
    missing and the 45 MB region is being re-hashed.
