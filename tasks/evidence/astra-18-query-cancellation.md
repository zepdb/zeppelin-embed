# Step 18: query cancellation — implemented

Base: `ee016e30ee80d839e92d127424fa17896786dc88`. This record accompanies the scoped Step18 implementation commit; its resolved identity is recorded in the execution log after commit.
The52-case focused checkpoint and seven directed plants pass. The matched
48-process ordinary API screen and six-process native cancellation campaign are
complete. Main source is unchanged since that checkpoint; overhead attribution and scoped source review are complete.
This is focused validation plus a64-query screen,not full qualification.

## Host measurements

Full host report and raw references:
[/private/tmp/ze-astra18-host-4b960428/REPORT.md](/private/tmp/ze-astra18-host-4b960428/REPORT.md).
The screen uses the complete57,638-document/58,980-chunk FiQA corpus and a frozen,
qrels-independent64-query selection. All3,072 timed whole-API calls have exact
before/after payload,score-bit,tokenizer,epoch and backend parity.

| State/API | Before p95 ms | After p95 ms | Change |
| --- | ---: | ---: | ---: |
| Intact dense | 1.282583 | 1.278833 | -0.29% |
| Intact Exact | 6.108875 | 6.131625 | +0.37% |
| Intact lexical | 3.455625 | 3.915916 | +13.32% |
| Intact hybrid | 6.580208 | 6.695750 | +1.76% |
| Deleted dense | 1.184083 | 1.234416 | +4.25% |
| Deleted Exact | 5.150667 | 5.114542 | -0.70% |
| Deleted lexical | 4.059292 | 4.434542 | +9.24% |
| Deleted hybrid | 6.425541 | 7.121875 | +10.84% |

Twenty warmups per process;AB/BA/AB independent repetitions;nearest-rank
quantiles within a process,median of three process quantiles. Both arms use the
same experimental clean71 adapters and CoreML64-token query model with normal
core features[]. Neither source nor storage treatment changes retrieval defaults.
The full report contains p50,individual p95 ranges,RSS,commands and source hashes.
The three regressions above5% were investigated with a separate diagnostic
loop-poll ablation. Removing only WorkCheck.step reduces lexical p95 by11.82%
intact/14.47% deleted and deleted-hybrid p95 by7.11%;all1,536 diagnostic
results retain exact payload parity. The diagnostic violates required cancellation
and is not retained. This establishes a material polling/code-generation cost,
not precise per-helper attribution. Step18 is retained for bounded cancellation
and avoided abandoned work,with its ordinary-query slowdown explicitly disclosed.

All384 main native probes cancel between instrumented wrapper entry and exit,
return typed cancellation with partial=false,and have matching clean recovery
results. CoreML residual p50/p95/max:0.804875/0.893750/0.957375 ms.
MLX residual p50/p95/max:4.355792/5.658708/7.778709 ms;its caller returns at
1.103375 ms p95 while the owned worker finishes later. All six extra close probes
join after worker completion. Native entry means the Rust wrapper around the
foreign call,not directly observed GPU/ANE instruction start. No universal
wall-time cancellation bound follows from these finite measurements.

The analysis-only JSON schema mismatch and exact correction are retained in
native-analysis-correction.json;all six runtime processes themselves succeeded.
BEIR preparation was paused at100,000 closed NQ documents for this exclusive
measurement window. The historical checkpoints below retain their original
measurement status; this section supersedes their earlier NOT MEASURED labels.

## Current verified checkpoint

Five additional preparation cases now have observed RED→GREEN in
`remaining-preparation-red2.json` and `remaining-preparation-green2.json`:

| Preparation work | RED operations | GREEN operations |
| --- | ---: | ---: |
| Owned term copies | 4,096 | 127 |
| Owned field copies | 4,096 | 126 |
| Term-query allocation-size walk | 4,096 | 127 |
| Weighted-query allocation-size walk | 4,096 | 127 |
| Live-statistics row walk | 4,096 | 127 |

The exact tested source is `remaining-preparation-green2-source.json` and its
26-file copy/patch. The clean term allocation is the literal 266,248 bytes;
weighted allocation is 450,568 bytes, checked against the legacy formula;
live statistics preserve 4,096 documents, 8,192 tokens and typed invalid-row errors.
The same checked sizing/copying paths now serve lifecycle prepared queries, and
contribution construction uses the controlled live-row walk after reserving memory.
Roaring cloning and allocation remain non-preemptible library operations.

`remaining-preparation-red1` failed compilation because a new test used `expect`
with a non-Debug error. `remaining-preparation-green1` failed compilation because
an existing test called the renamed private field helper. Neither is behavioral
evidence. Both were corrected before the terminal receipts above.

The public contribution-cancellation test then exposed a missing handoff:
legacy `Store::search_lexical` passed `None` to assembly. Its actual RED in
`remaining-preparation-public1.json` completed 4,096 live rows and retained an
8,808-byte completed assembly before returning the expired deadline. Passing the
existing cancellation into assembly gives 127 rows, cache 0→0, temporary 0 and
identical fresh-query candidates/generation. The same test plus three affected
prepared-budget, retained-assembly and structured-query controls pass in
`remaining-preparation-public2.json`; source is captured in
`remaining-preparation-public2-source.json` and its copy/patch.

The public scoring-handoff test also exposed a missing control path. Its intended
RED (`public-scoring-red1.json`) consumed all 4,097 DF document IDs before timeout.
The legacy lexical planner now passes the original cancellation through validation,
allow-list scoring and pruned scoring, including field preparation and live DF.
The identical GREEN (`public-scoring-green1.json`) consumes zero DF document IDs
after the fixture expires at corpus-statistics preparation. Both observations
retain the complete 15,576-byte assembly, zero temporary bytes, and fresh-query
candidate parity. This is an admission/work result, not a native latency result.

The public-scoring checkpoint has **45 passing cases**: 34 core Step18 cases, four
planner/statistics regressions, and seven text-control cases. Commands and terminal
exits are in `public-scoring-checkpoint1.json` and `public-scoring-checks1.json`.
Normal-feature core Clippy exits zero with the 25 inherited warnings. Earlier
native controls remain recorded at their own source checkpoint. Cold active
sealing was audited next, with the results below. The separate clean71 graph
remains stopped. BEIR SciFact, FiQA and TREC direct-query campaigns are complete;
their exclusive windows are released and NQ preparation has resumed.

## Cold active preparation checkpoint

Five further exact tests observed their intended RED in `cold-sealing-red1.json`
and identical GREEN in `cold-sealing-green1.json`, with no compiler/fixture repair.
The original RED source is preserved in `cold-sealing-red1-source.json` and its
28-file copy. Each multi-phase test ran every phase before checking the work bound.

| Work | RED probes | GREEN probes |
| --- | ---: | ---: |
| Position-gap encoding | 4,096 | 125 |
| Encoder bit-width calculation | 4,098 | 125 |
| Bit packing | 4,097 | 123 |
| Block-impact rows | 4,096 | 127 |
| Active field-length copying | 4,096 | 126 |
| Active total-length preparation | 4,096 | 125 |
| Multi-field posting union | 8,194 | 64 |
| Public cold active query, field-length rows | 4,096 | 126 |

The public RED retained a completed active lexical cache, increasing active bytes
672,100→712,020 before timeout. GREEN retains 672,100 bytes, cache 0→0 and temporary
0, publishes no partial assembly, and a subsequent uncanceled query matches the
independent full reference store's candidates and generation.

Controlled sealing now reaches row copies, totals, field lookup, impact creation,
posting encoding, metadata validation, unions and byte copies. It checks again
before publishing the active cache. Host byte copies use 8,192-byte chunks and
preserve the previous reserve policy; allocator calls remain non-preemptible.
Active text-presence and tombstone walks are controlled. Assembly sorting uses the
previously tested fallible sort; public lexical allow-list and candidate copying
also check the original deadline. Source error/byte/accounting contracts remain.

`cold-sealing-checkpoint1.json` records **45 passing core cases**: 39 Step18 tests
plus six affected regressions. The latter preserve v1/v2 postings goldens, the full
postings-region golden, all bit widths, active-cache invalidation and refused
contribution accounting. Normal-feature core Clippy exits zero with the 25 inherited
warnings. `cold-sealing-text1.json` adds seven passing text/runtime control cases,
giving **52 passing focused cases** at the current source. Earlier real-native
controls remain separately recorded. Final source review and host measurements
are pending. The exact 29-file source is `cold-sealing-checkpoint1-source.json`
and its preserved copy/patch.

The two earlier public handoff plants both fired and their identical restored
controls passed; byte-exact restoration is in `public-handoff-plants1/`. The cold
active handoff plant also fired and its identical restored control passed, with
byte-exact restoration in `active-seal-plant1/`. These duplicate directed runs are
not included in the 52-case count above. All test/plant processes are terminal.

## Previous position/worker checkpoint

The position/worker checkpoint now has **41 passing cases**: 27 core Step18 tests,
7 text/MLX-control tests, five affected position/phrase/candidate regressions, one
real MLX batch-boundary reference test and one real TextStore tier/control test.
Its exact source is `position-worker-checkpoint1-source.json` plus the source copy
and patch; it superseded the older dictionary-only checkpoint at that source.
The original v1/v2/v3 prepared sources and all prior checkpoints remain preserved.

The thirteen prepared commands ran to terminal results in `position-worker-red1.json`:
eight core cases and the actual global MLX mutex case failed for their intended
work/admission assertions. Saturation, close/join, late completion precedence and
panic recovery already passed. After the scoped repairs, all thirteen identical
commands passed in `position-worker-green1.json` without a compiler/fixture repair.

| New bounded-work case | Observed RED | Observed GREEN |
| --- | ---: | ---: |
| Position metadata probes | 4,096 | 127 |
| Row membership probes | 4,096 | 127 |
| One-row position decode | 4,096 positions / 512 bytes | 127 positions / 16 bytes |
| Prepared/public phrase decode | 4,096 positions / 512 bytes | 122 positions / 16 bytes |
| Candidate routing | 12,491 comparisons; one stream opened | 83 comparisons; zero streams |
| Duplicate-result copies | 4,096 | 127 |
| Assembly and MLX admission | Waited until holder unlocked | Timeout returned while holder retained lock |

The public phrase timeout retained zero temporary bytes and unchanged cache charge
(618 bytes); its fresh control preserved candidates, expansions and generation.
The saturation fixture has one running stand-in native call, two canceled queued
commands and one expired blocked admission. Only the first native call executes;
close drains/skips the queued commands and joins. These are deterministic work and
occupancy observations, not measured native residual or scheduling latency.

Four deliberate worker plants fired, with each identical restored control passing:
removing the queued-start check, preferring a native error over cancellation,
disabling the existing panic site, and detaching the native worker instead of
joining it. Full source patches, commands, seed `0x5eed`, logs and byte-exact
restoration are under `worker-plants1/`. The close plant specifically failed the
join-ownership assertion; no compile failure is counted as a firing plant.

TREC released the machine after all five build processes exited zero. Parent ran
these tests without competing task-owned measurements and has now returned the
window for TREC reference/calibration/query work. Graph and NQ remain paused.
ASTRA-ISSUE-019 records the separate bulk-ingestion traversals; no Step18 repair
or CPU-time attribution for that issue is claimed.

The deferred overhead protocol and script are in `measurement-preparation/` under
the artifact root. They specify 64 independently selected query IDs on the full
FiQA corpus, intact/deleted stores, all four APIs and 48 alternating processes.
Python syntax and historical Rust replacement anchors were checked. Preparation,
source archives, store copies, builds and measurements for that screen are NOT RUN.
The script requires final source/test receipts; this intermediate checkpoint is
not permission to bypass the remaining Step18 work.

## Implemented so far

The additive controlled TextStore entry carries one absolute QueryControl through
admission, embedding, retrieval and returned-text construction. Stopped queued
commands skip model evaluation; late native output is suppressed. Controlled
send/reply waits poll at 1 ms; that interval is not a measured scheduling bound.

WAND, MAXSCORE, short-list and weighted scoring use cooperative work checkpoints.
The same counter reaches TermStream open/reset/seek/advance/TF and bound operations,
including internal zero-effective-TF walks under fractional field weights. The
lifecycle prepared-query route, candidate rescoring, posting-driven allow lists,
weighted ranking and structured-hit provenance use the controlled streams.

Weighted document lengths now check row and field-table work. The unit single-field
and full-coverage multi-field cases still borrow the precomputed length arrays.
Prepared term/weighted/candidate statistics propagate control through exact live
DF and strict-subset field unions. Interrupted DF publishes no cached value;
controlled DF cache admission can stop while another thread holds its mutex.

Term/phrase expansion copying, prefix enumeration, rejected fuzzy candidates and
banded fuzzy DP now check the same query control. The prepared phrase matcher calls
controlled alignment DP, including the potentially long predecessor-minimum scan.
These additions preserve scoring/boost/order/slop semantics.

Vocabulary count/copy/group/length-index construction, phonetic map construction,
long-term normalization/encoding, and phonetic bucket selection now carry the same
control. An allocation-free fallible heapsort checks comparisons and moves for
dictionary, phonetic-map and accepted-fuzzy sorting. Aborted sorting retains a valid
permutation and never publishes partial order. Comparisons remain work units, not
a universal byte-level or wall-time bound; default overhead is still unmeasured.

Vocabulary and phonetic cache mutex admission now polls the original deadline.
Construction retains the existing reservation, ownership and publication order.
Public query tests prove interrupted cold builds publish no partial value, retain
no extra cache charge, and release all temporary memory. Position readers now share controlled metadata/membership/decode work with phrase
preparation and alignment, while retaining the same reservation and structural
validation contracts. Candidate routing, grouping and output copies also check.
Assembly and actual process-wide MLX admission use the original control while
waiting. RuntimeSet/QueryRuntime route controlled queries through this MLX seam;
plain ingestion retains blocking admission and the shared bounded evaluator.
Each foreign chunk finishes under its guard before a post-call control check;
foreign execution is not claimed preemptible. Remaining preparation gaps and
native residual/overhead measurements below remain explicit.

## Observed RED and GREEN

All rows below are deterministic work observations, not latency benchmarks.
The current interrupted counts include the newly instrumented nested operations.

| Case | Prior observed work | Current interrupted work |
|---|---:|---:|
| WAND scored postings, 4096-row/four-term fixture | 16,384 | 26 |
| WAND metadata probes, no-scoring fixture | 257 | 30 |
| Exact live DF, 4096-row fixture | 4,096 postings / 64 blocks | 62 postings / 1 block |
| Fractional stream open/reset | 4,095 zero-TF rows | 63 |
| Fractional stream seek/advance | 4,094 zero-TF rows | 63 |
| Fractional weighted-length preparation | 4,096 rows | 42 |
| Prefix expansion | 4,096 terms | 128 |
| Fuzzy expansion with no accepted matches | 4,096 candidates | 1 completed distance |
| One long fuzzy distance | 20,474 DP cells | 103 |
| Phrase final predecessor scan | 4,096 probes | 56 |
| Preparation sort | 51,190 comparisons | 83 |
| Vocabulary count walk | 8,192 total iterator visits before final cancellation | 64 visits / 0 reservations |
| Phonetic map build | 4,096 encodings / 1 completed build | 64 encodings / 0 completed builds |
| Long-term normalization / encoding loop | 4,096 probes in each targeted phase | 127 in each phase |
| Phonetic selection from one bucket | 4,096 copied terms | 127 |
| Expired/canceled queued native command | 1 fake native call | 0 |

MAXSCORE stops after 19 scored postings. Its original baseline was not independently
recorded because the original RED failed first in WAND. The metadata clean control
still visits 257 probes with no scoring. The DF clean follow-up returns 2,048,
publishes one complete cache value and reuses it without another DF walk.

The phrase test allows the first 4,096 predecessor probes to finish, then expires
inside the next term's predecessor scan. Total work is 8,192 → 4,152, with the clean
literal minimum displacement remaining 4,095. Its RED already returned cancellation
at the final stage check, but failed the in-loop work bound; it did not prove prompt
cancellation. The same named command reached GREEN after checks entered the scan.

The fuzzy test records partial DP work even when an inner checkpoint returns an
error. Its clean follow-up reuses the scratch object and returns exact distance 1.
Prefix/fuzzy clean controls retain the complete expansion output, including an
empty result when every fuzzy candidate is rejected.

The vocabulary copy-phase GREEN completes the first 4,096 count visits and stops
at 4,160 total visits with one reservation. The original RED stopped at the first
phase assertion, so no independent copy-phase RED is claimed. Clean controls retain
complete sorted vocabulary/field membership, length ordering and phonetic output.

Both vocabulary and phonetic deadline tests returned only after lock release in
the intended RED, then returned Timeout while the holder still owned the mutex in
GREEN. Barriers and a fake clock establish the ordering: entry sees the original
unexpired deadline, then that same deadline expires before admission. The watchdog
only detects failure; it does not supply a latency measurement.

Public cold-dictionary timeout occurs after 16,385 grouping checks, leaving cache
bytes 724 -> 724. The cold-phonetic timeout stops at 64 encodings with its already
complete dictionary retained, cache bytes 463,771 -> 463,771. Both return typed
Timeout with partial=false, leave temporary bytes zero, publish no partial cache,
and permit a subsequent successful uncanceled query. This integration proof was
added after underlying RED/GREEN; it has no separately observed original RED.

## Receipts and source identities

Artifact root:
`/var/folders/z_/fscz84rs53z5_2klsmkmv2vw0000gn/T/ze-astra18-kdtldk5q`

- Original REDs: `pruned-red.log`, `metadata-red.log`, `df-red2.log`; earlier queue,
  expired-entry and join/precedence receipts remain intact.
- `fractional-stream-red2.log` is the final natural fixture RED for all four stream
  seams; `fractional-stream-green3.log` is its GREEN. The initial red fixture and
  green1/green2 compile failures are retained, not counted as behavioral success.
- `weighted-length-red.log` and `weighted-length-green1.log` show the identical
  weighted-preparation test fail and pass.
- `expansion-red.json` and `expansion-green1.json` contain exact commands for the
  separate long-distance and prefix/rejected-candidate RED/GREEN tests.
- `phrase-dp-red.json` and `phrase-dp-green1.json` identify the phrase DP RED/GREEN.
- `nested-green.json` retains the earlier 14-case checkpoint, including both
  weighted literal/bound cases, repeated candidate terms/fields, borrowing and
  public structured provenance. `nested-green-source.json` identifies its source.
- `expansion-phrase-final.json` contains the previous checkpoint's commands/exits.
  `expansion-phrase-final-source.json` and `expansion-phrase-final-source/` preserve
  the exact code hashes and copies, including new source files. Hashes rechecked.
- `df-red.log` was an earlier compile failure; the initial precedence SendError was
  a test notification defect. Neither is product RED. Old evidence remains in
  `REPORT-before-expansion.md`, `core-green2.json`, `focused-neighbors2.json` and
  `core-checkpoint-source/`; it is not represented as validation of later changes.
- `dictionary-red.json` and `dictionary-green1.json` contain the sort, vocabulary,
  phonetic construction and long-encoding commands/exits. The first cache-wait and
  selection attempt failed to compile (E0373 test closure capture); that is not
  behavioral RED. `dictionary-waits-red2.json` contains their intended RED, and
  `dictionary-waits-green1.json` contains GREEN plus the public cache-abort proof.
- `dictionary-final.json` records the prior dictionary validation commands/exits.
  `dictionary-final-source.json`, `dictionary-final-source/` and
  `dictionary-final-checks.json` retain exact source, hashes and final checks.
  Previous active evidence/state are preserved as `REPORT-before-dictionary.md`
  and `STATE-before-dictionary.json`.

## Current focused validation

`position-worker-checkpoint1.json` records the eight focused/regression commands
(40 executed cases); `position-worker-checks1.json` adds normal-feature library
Clippy and the one-case real TextStore control. Both crates' Clippy exited zero
with the existing 25 core / five text warnings. Format and diff checks passed.
`position-worker-checkpoint1-checks.json` records these checks and preservation
of HEAD and the unrelated Python example. No ignored or zero-match case counts.

The real `arctic_v15_batched_cls_matches_reference_across_padding_and_chunk_boundary`
case passed through the shared MLX evaluator for single, padded two-row and 33-row
batches. The real `astra_06_text_query_exposes_explicit_rescore_and_preserves_tier_controls`
case passed dense, Exact, lexical and hybrid controls through TextStore. These
establish native behavioral execution, not a native cancellation latency result.

## Previous dictionary validation

Twenty-nine cases passed at the captured dictionary source checkpoint, with no ignored/zero-match
claim. Exact commands for each named neighbor appear in `dictionary-final.json`:

```sh
cargo test -p zeppelin-embed --lib astra_18_ -- --nocapture
# 19 cases
cargo test -p zeppelin-embed --lib astra_12_prefix_field_membership_and_byte_edges_preserve_expansion_order -- --nocapture
cargo test -p zeppelin-embed --lib astra_12_vocabulary_groups_memberships_with_linear_work -- --nocapture
cargo test -p zeppelin-embed --lib astra_12_vocabulary_charge_survives_eviction_and_close -- --nocapture
cargo test -p zeppelin-embed --lib astra_12_refused_vocabulary_is_not_published -- --nocapture
cargo test -p zeppelin-embed --lib astra_14_phonetic_cached_expansions_match_full_scan -- --nocapture
cargo test -p zeppelin-embed --lib astra_14_phonetic_empty_invalid_and_multifield_cases_preserve_errors -- --nocapture
cargo test -p zeppelin-embed --lib astra_14_phonetic_cache_tracks_vocabulary_and_algorithm_identity -- --nocapture
cargo test -p zeppelin-embed --lib astra_14_refused_phonetic_index_is_not_published -- --nocapture
cargo test -p zeppelin-embed --lib double_metaphone_encodings_match_the_committed_name_list -- --nocapture
cargo test -p zeppelin-embed --lib astra_13_fuzzy_bounded_matches_full_distance_expansions -- --nocapture
# 10 separate one-case neighbor runs above
cargo clippy -p zeppelin-embed --lib
# exit 0, 25 inherited normal-feature warnings; no new task warning
```

The earlier 16-case expansion/phrase checkpoint and queue/TextStore tests passed at
their recorded source checkpoints; they are retained separately, not rerun here.
The newer native-lock and directed worker-fault evidence is recorded above.
Formatting and diff checks are recorded separately. No full suite, broad adversarial
campaign, timing comparison or Step18 commit is claimed by this checkpoint. BEIR
preparation may overlap these untimed tests; authoritative timing windows are
coordinated separately and did not overlap this work.

## Disposition and qualification

The required cancellation repair is retained. The52-case source checkpoint,seven
firing/restored directed plants,48 ordinary API processes,six native-residual
processes and24 diagnostic attribution processes are complete. No production
source changed after the recorded final focused checkpoint. The final source
review covers original deadline/error precedence,owned queue/join lifetime,FTS
loop checkpoints,cache publication/rollback,and frozen wire/accounting controls.

The cost is recorded as ASTRA-ISSUE-020: ordinary lexical p95 increases9.24–13.32%
and deleted-hybrid p95 increases10.84%. The measured tradeoff is not a claimed
search speedup. Allocation,system/storage operations and foreign model execution
remain non-preemptible intervals;observed native residuals are not universal bounds.

Full648-query confirmation and broad suite qualification remain the shared
checkpoint/final-matrix work,not implied by this64-query screen. The user's
current stopping instruction is to complete Step19 and then stop this plan;
Steps20–33 must not start. Main model/default settings and prior clean71
worktree/artifacts remain preserved.
