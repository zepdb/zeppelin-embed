# Bug-bash execution plan

**Assessed against:** `main` at `9ba0734` (working tree carries the staged
task-21 D6+D9 merge — epoch transition, embedder seam, I14 — which is about
to be committed; nothing here touches it). Assessment date 2026-08-25.

**Live update 2026-08-25:** BL-160 is ruled and implemented on
`api/bl160-r15`: no hybrid tier preference selects exact scoring, an explicit
tier still wins, and estimated vector scores still fail with
`FusionError::EstimatedVectorScore`. R15's remaining fusion header and local
decision ledger are also implemented there. Inventory counts below are the
assessment-time snapshot, not current status.

**Inventory:** 110 open items — 95 `BL-*` from `tasks/execution_order.md`
plus R01–R15 from `tasks/recommendations-from-codex.md`, read through
`tasks/assessment-of-codex-recommendations.md` (same-day, source-verified).

**Counts.** By priority: 4×P0 (BL-134, R01, R02, R03), 39×P1, 50×P2, 17×P3.
By type tag (an item may carry two; counted once per tag; 12 items carry
two tags): [correctness] 8 · [perf] 33 · [test-gap] 48 · [api] 6 ·
[format] 1 · [hygiene] 16 · [owner] 10.
By disposition: **24 batched DEFER-OK** (batches 1–8), **21 batched after
CP1** (15 in the VERIFY-NOW engine sequence, 3 hygiene in batch 23, 3
coverage in batch 24), **33 PROPOSED CLOSE**, **25 campaign items** (24
distinct — R09 duplicates BL-149), **6 owner decisions** (BL-008, BL-090,
BL-100, BL-104, BL-160, R14; three more forks gate batches 16/17/21), and
**1 re-read** (BL-159, retired by batch 11). Total 110.

**Honesty statement.** Every P0, every proposed close, and the first ten
batches were verified against source in this pass (function-level reads;
citations below). The R items lean deliberately on the same-day assessment,
which itself verified all fifteen premises against source; I independently
re-confirmed R01 (no `stored_metadata`/`postings` read in `graph/build.rs`,
no unlink in `tier/maintain.rs`), R02 (`segment/reader.rs:515-531` whole-region
`xxh3_64` per row lookup), R04 (`hybrid_vector_candidate_limit` = total corpus
rows), R06 (`sift_1m()` hardcoded at `tier/maintain.rs:187,321`), R07
(`MAINTENANCE_CHECKPOINT_ROWS = 64`), R09 (`select_strategy` ignores `k`),
and R12 (`RuleSignals::none()` at `fusion/mod.rs:146`). Roughly 30 P2/P3
tail items were classified from entry text plus one targeted grep only; each
is marked "pass 1" in the item table. Nothing marked "pass 2" was closed or
scheduled without the cited code being opened.

**The single most useful finding:** a large slice of this backlog is already
fixed and nobody closed the entries. 33 items are proposed for closure with
evidence, including four P1s that only needed a `git log` read (BL-153,
BL-154, BL-139, BL-141) and a family of test-gap P1/P2s that the 19-M4
latency-trim and 09-C follow-up campaigns silently repaired (BL-118, BL-126,
BL-095, BL-105, BL-108, BL-109, BL-096, BL-099, BL-069). Killing these first
removes about 30% of the apparent workload.

---

## Tag legend

| tag | meaning |
| --- | --- |
| [correctness] | a user could observe a wrong result, wrong ranking, lost data, or a broken durability guarantee |
| [perf] | behavior is correct, cost is not |
| [test-gap] | the test or instrument proves less than it claims, or cannot fail |
| [api] | public surface, FFI shape, or an interface seam |
| [format] | persisted layout, versioning, or goldens |
| [hygiene] | docs, naming, dead code, stale comments |
| [owner] | needs a human product or interface decision, not a fix |

Tags are independent of priority. Two divergences worth naming: **R02/R03
are P0 yet [perf]** — no wrong answer, but the cost (whole-region re-hash
per row lookup, per query) dominates every warm query; and **BL-106 is P3
yet [perf]-on-attacker-input** — a real O(n²) on crafted bytes that nobody
hits in normal use.

---

## 1. The batch plan

Each batch is one commit. Order is execution order. "RED" names the test
that must be observed failing (or the mutation that must be observed caught)
before the fix lands.

### DEFER-OK block — batches 1–8, safe to stack, one validation run at CP1

These touch only docs, test code, bench tooling, and CI scripts. No engine
code path changes. This is the block that genuinely supports
"fix many, validate once." **It holds 24 items. That is the honest ceiling:
every remaining open defect sits on the query path, the maintenance path,
a persisted artifact, memory accounting, or the C ABI — exactly the layers
where a silent break cannot be attributed by a later suite run — so
everything after CP1 is VERIFY-NOW by construction.**

**Batch 1 — Documentation truth pass** [hygiene]
Items: BL-085 [hygiene], BL-084 [hygiene], BL-094 [hygiene], BL-049
[hygiene], BL-091 [hygiene], BL-121 [hygiene], BL-033 [hygiene], R15
[hygiene] (the two safe citations now: `fts/bm25.rs:128-131` manifest claim,
`fts/prune/mod.rs:21-28` superseded rule, plus the `tasks/17` status line;
hold the `fusion/mod.rs` header for Batch 22, per R15's own sequencing note).
Files: `tasks/execution_order.md:290` (stale BL-026 sentence — verified
still present), `tasks/evidence/*` supersession pointers, root `CLAUDE.md`
Quickstart coverage line (verified still wrong), `scripts/coverage.sh`
comment, `tasks/19/20` crossover numbers, `kernels/mod.rs:230-263` (Bit4Row
invariant doc must name both constructors — verified stale; the
`from_validated_bytes` second constructor at `:263` contradicts the "only
constructor" sentence a SAFETY comment leans on), size-trajectory note.
DEFER-OK: prose only. RED: n/a — acceptance is `RUSTDOCFLAGS="-D warnings"
cargo doc` staying green at CP1. Cost: S total.

**Batch 2 — Scope the Unicode-3.0 allowance** [hygiene]
Items: BL-140 [hygiene]. Verified: `deny.toml:11` still allows Unicode-3.0
globally rather than as a per-crate exception like `xxhash-rust`'s BSL-1.0.
Files: `deny.toml` (+ optional CI note-check). DEFER-OK: policy file.
RED: already proven by the entry — removing the allowance fails
`cargo deny check` on `unicode-ident`; after scoping, the same check must
pass and a planted second Unicode-3.0 crate must fail. Cost: XS.

**Batch 3 — Test hermeticity** [test-gap]
Items: BL-156 [test-gap], BL-161 [test-gap][format], BL-003 [test-gap].
Verified: `ffi_header.rs:101` still shells a nested
`cargo build -p zeppelin-embed-ffi --release` with no private target dir,
so a coverage run poisons `target/release` and the gate cannot run twice;
`tests/corruption.rs` now derives *segment region* offsets from the encoded
bytes (`entry_offset`/`region_offset` helpers) but still lacks the
changed-the-byte-it-intended guard BL-161 asks for on manifest mutations.
Files: `crates/zeppelin-embed-ffi/tests/ffi_header.rs`,
`crates/zeppelin-embed/tests/corruption.rs`, `tests/` size-budget test.
DEFER-OK: test-only. RED: (BL-156) a new
`header_gate_passes_twice_in_a_row_after_an_instrumented_build` test —
currently reproducibly RED per the entry's three measured runs; (BL-161) a
guard asserting each mutation changed the intended field — prove it by
shifting one offset 4 bytes and watching the guard fire (the entry already
demonstrated the different-typed-error failure mode); (BL-003) tighten the
assertion, prove by inverting the gate. Cost: S.

**Batch 4 — Deterministic graph cancellation test** [test-gap]
Items: BL-120 [test-gap]. Verified: `graph/search.rs:2359-2374` still
spawns a 2 ms sleeping thread and asserts `elapsed < 25ms` — a nonzero
flake budget against the zero-budget rule. Fix per the entry: keep the
cross-thread visibility test, add a deterministic pre-cancelled entry-check
case, drive the cancel from a counting token after N hops. Files:
`crates/zeppelin-embed/src/graph/search.rs` (test module only). DEFER-OK.
RED: the counting-token test does not exist; write it against a token that
never fires and observe the wrong variant. Cost: S.

**Batch 5 — Adversarial I13 truthfulness** [test-gap]
Items: BL-158 [test-gap], BL-137 [test-gap]. Verified:
`tests/adversarial/runner.rs:687` still hardcodes
`diagnostics_plan_matches_execution: true` on the unfiltered path while
`:763` computes it for filtered; the BL-137 "no graph built" honesty check
appears rewritten (runner now gates on the `GraphNodeBlocks` region at
`:631` and `SealedGraph` plan tier at `:721`) but the owed seed re-run is
unrecorded. Fix: an independently observed execution trace for the
unfiltered arm (needs a second source beside `diagnostics.plan` —
structural, per the entry), then re-run the previously affected seeds and
record the verdicts, closing BL-137. Files: `tests/adversarial/*.rs`.
**Coordinate with the staged I14 work before touching these files.**
DEFER-OK: harness only. RED: plant a plan/execution mismatch on an
unfiltered search and observe I13 stay green today. Cost: M.

**Batch 6 — R13: the store-seam baseline benchmark** [test-gap]
Items: R13 [test-gap]. New bench binary at the public `Store` seam with
checksum-bytes and allocation counters (diag already carries ScanStats,
GraphSearchStats, lexical counters, plans, QoS, epochs). Files:
`crates/zeppelin-embed-bench/src/bin/` + counter plumbing in bench crate.
DEFER-OK: bench crate only. **Sequence-critical despite being DEFER-OK: its
baseline numbers must be banked before batches 10, 12, and 22 merge, or
there is no honest before/after.** RED: n/a (new instrument); acceptance is
a `tasks/evidence/` baseline file naming hardware, command, raw numbers.
Cost: M.

**Batch 7 — Measurement-infrastructure honesty** [test-gap]
Items: BL-125 [test-gap], BL-025 [test-gap], BL-103 [test-gap], BL-119
[test-gap], BL-101 [test-gap], BL-070 [test-gap]. Verified where cheap:
`graph-search.rs:266` still hands itself
`with_observed_core_class(QueryCoreClass::Performance)`; `b1-measure.rs`
RESULT lines carry provenance but no visible non-bench-profile refusal
(pass-2 partial — confirm before annotating). Work: replace the matrix
load1 gate with short-window non-idle-CPU sampling; preflight fails closed
on concurrent cargo/rustc; DRAM gates become across-process medians;
re-run `calibrate_core()` after the measured loop or drop the field; keep
the DRAM prefetch pair as a recurring measurement (note: runtime
`TraversalPrefetch` A/B arms now exist in `graph/search.rs:216`, so most of
BL-101's ask is wiring, not building); annotate exactly the three void
27-B1 rows and add the profile+codegen-honesty guard. Files:
`tasks/cross-benchmark/run_matrix.py`, `zeppelin-embed-bench/src/**`,
`tasks/evidence/opt-ledger`. DEFER-OK: tooling only. RED: per-item — e.g.
the load-gate change is proven by the entry's own self-poisoning trace;
the core-class fix by deleting the hardcode and watching nothing fail
today. Cost: M.

**Batch 8 — Gates that assert instead of hope** [test-gap]
Items: BL-023 [test-gap], BL-001 [test-gap]. Work: per-kernel roofline
floors + cross-kernel ratio invariants wired into `scripts/ci-gates.sh`
(floors from measured values with ~5x headroom, each citing its evidence
row — the owner-decided design constraints are in the entry); a
linked-size measurement of a minimal consumer binary beside the archive
gate. Files: `scripts/ci-gates.sh`, `scripts/size-budget.sh`, a tiny
consumer target. DEFER-OK: scripts. RED: plant a 10x-slowed kernel arm
(feature-gated) and watch the floor fire; set the floor above a real
number and watch it fail for the right reason. Cost: M.

**>>> CHECKPOINT 1 (see §2) — validate batches 1–8 in one run. <<<**

### VERIFY-NOW sequence — batches 9–22, each validated before the next stacks

**Batch 9 — R01 containment guard: stop the data loss today** [correctness]
Items: R01 (guard slice) [correctness]. Verified: `graph/build.rs`
`write_segment_with_graph` never reads `input.postings()` or
`input.stored_metadata()` (zero grep hits in the file), no graph-writer
variant with those slots exists (`segment/writer.rs:639-661` — variants
stop at `_with_graph_and_documents`), and `transition_due` promotes on
row-count alone — so any text-bearing segment past 10,000 rows silently
loses its BM25 postings and stored metadata on `maintain()`. The guard:
`maintain_one` refuses/defers promotion of any segment holding a region the
rewrite cannot carry, with a typed report. One file, fail-loudly, no format
change. VERIFY-NOW: maintenance path; run the crash matrix and
`purge_proof` suites. RED:
`maintain_defers_promotion_of_segments_whose_regions_it_cannot_carry` —
build a text+metadata segment past `graph_min_rows`, call `maintain()`,
assert the typed deferral; today it silently promotes and the postings
vanish. Cost: S. **This lands before everything else on the engine because
it converts a silent wrong-lexical-results bug into a loud no-op.**

**Batch 10 — R02: validate the identity region once per reader** [perf]
Items: R02 [perf]. Verified: `segment/reader.rs:515-531` runs `xxh3_64`
over the complete DocumentVersions region on every single-row
`document_version` call; hot callers include the hybrid/lexical join
loops, the graph-tier merge, every scan-tier candidate, and — worst —
`graph/build.rs` per row, making every promotion O(N²) in hashed bytes.
Fix: once-per-`SegmentReader` validation, then bounds-checked 24-byte
reads; add byte counters. Files: `segment/reader.rs` + counter surface
(1–2 files). VERIFY-NOW: read-path checksum semantics — corruption tests
must still fire on a corrupt region. RED:
`document_version_hashes_the_region_once_per_reader` — deterministic
checksum-bytes counter across k lookups equals region size, not
k×region-size; currently RED by construction. Requires the Batch 6
baseline banked first. Cost: S–M.

**Batch 11 — BL-134: one score scale per merged answer** [correctness][api]
Items: BL-134 [correctness][api]; re-reads BL-159 [test-gap] afterward.
BL-160 was subsequently ruled exact-by-default for hybrid with no explicit
tier. Verified: `segment::writer::write_segment` is public;
under Auto, graphless segments are exact-scanned
(`lifecycle/mod.rs:1935-1942` via `rescore_f32`/`scan_squared_l2`) while
Bit4 scan segments emit estimates, so a `scheme: 0` F32 segment published
beside a graphless Bit4 one merges two score scales in one heap. **The fix
is query-side — snapshot-wide tier selection or exact rescore before the
global merge. A writer-side fix is forbidden: `tests/corruption.rs`
constructs `SegmentBuild` directly and would break.** Files:
`lifecycle/mod.rs`, `planner/exec.rs`; no format change. VERIFY-NOW: the
query path itself. RED: `auto_never_merges_estimated_and_exact_scores` —
publish the mixed pair through the public writer, query Auto, assert either
a single scale or a typed refusal; today it merges silently. After it
lands: re-read BL-159's I13 bound note and retire it if the planner can no
longer be wrong about `approximate`. Task 22 consumes the later BL-160 ruling;
it does not reopen the score-scale decision. Cost: M–L.

**Batch 12 — R03: snapshot-lifetime decoded views, exactly accounted** [perf]
Items: R03 [perf] (+ the per-query Roaring `alive_bitmap().clone()` counter
the assessment flagged). Per-snapshot caches for alive/postings/columns/
rescore-validation; `Store::stats()` must cover retained decoded bytes per
Task 09's exactness rule; corrupt-region behavior stays typed and loud.
Files: `segment/reader.rs`, `fts/sealed.rs`, `planner/exec.rs`,
`lifecycle/*` (4–8 files). VERIFY-NOW: memory accounting invariants +
read path. RED: counter tests per region — decode/hash bytes across two
identical queries must not double; currently RED. Baseline from Batch 6
required. Cost: L.

**>>> CHECKPOINT 2 — full workspace run before the maintenance rewrite. <<<**

**Batch 13 — R01 full: region-preserving promotion + unlink** [correctness]
Items: R01 (full) [correctness]; closes BL-147 [correctness] and BL-148
[correctness] as subsumed duplicates. The rewrite carries postings,
stored-metadata, and unknown regions forward through graph promotion
(copy-forward contract for region kinds the writer does not understand),
then `publish_transition` unlinks the replaced `segment-<id>.zseg`
(verified: no unlink/remove in `tier/maintain.rs` today, so a complete
copy of dropped partitions survives `drop_partition` — the deletion
guarantee this product differentiates on). No new region kinds, no format
version bump. Files: `graph/build.rs`, `segment/writer.rs`,
`tier/maintain.rs` (3–5 files). VERIFY-NOW: persisted artifacts + crash
matrix; the deletion guarantee. RED:
`graph_promotion_preserves_stored_metadata_and_postings` (postings()
currently None after maintain — the Batch 9 guard's deferral is lifted by
this batch) and `publish_transition_unlinks_the_replaced_segment` (file
currently survives). **Batches 9 and 13 are welded: reverting 13 must
re-arm 9's guard, or the data loss returns silently. Revert them as a
pair or not at all.** Cost: L.

**Batch 14 — One canonical exact-L2 kernel** [perf]
Items: BL-128 [perf], BL-115 [perf]. Verified: `lifecycle/mod.rs:2164-2225`
`scan_squared_l2` is a scalar per-element f64 loop on the caller thread
(active segment + every graphless segment under Auto/Graph), and the graph
rescore's L2 arm is the same shape one layer down
(`quant/rescore.rs:358` → `squared_l2_f64`; note BL-115's original
citations moved here in the latency-trim refactor — the substance
survives, the line numbers did not). Fix: one SIMD squared-L2 kernel
beside `dot_f32`, both callers routed through it, local loops deleted.
VERIFY-NOW: score reassociation — BL-115's catch is binding: run the
recall gate BEFORE merging (margin over the floor is 0.0041), accept only
if recall@100 moves < 0.0005. RED: existing brute-force-equality tests
define the contract; the perf floor from Batch 8 is the regression guard.
Files: `kernels/neon.rs`, `quant/rescore.rs`, `lifecycle/mod.rs`. Cost: M.

**Batch 15 — Scan wrapper remedies** [perf]
Items: BL-073 [perf]. Inline `BoundedTopK::push` + widen dispatch to ≥256
rows (57% of the measured 2.35 ns wrapper gap). Unblocked: BL-069's fixture
fix has landed (verified — `benches/scan.rs` honors
`--fixture degenerate|clustered|random`, random default), so validate on
`random` and `ascending`, never `clustered`, per the entry. Files:
`kernels/`, `scan/`. VERIFY-NOW: hot-path kernels; perf floors + equality
proptests. RED: the Batch 8 floor plus bit-identical scores across the
dispatch-width change. Cost: S–M.

**Batch 16 — R06: epoch-keyed graph profiles** [correctness]
Items: R06 [correctness]. **LANDED at `b256c71` + `a67f3d4`.** Auto query
and maintenance now resolve the same graph profile from the persisted epoch's
document/query normalization and metric. An unnormalized squared-L2 epoch
selects `SiftClass`; a normalized shape has no owner-approved profile and
returns the typed `GraphProfileError::UnrecognizedEpochShape` instead of
silently using SIFT. Executable evidence:
[`auto_profile_follows_epoch_normalization` and
`genuine_sift_epoch_selects_sift_class`](../crates/zeppelin-embed/tests/store_graph_search.rs),
and [`maintain_builds_with_the_same_profile_auto_query_selects`](../crates/zeppelin-embed/src/tier/maintain.rs).
The separate recall-evidence campaign remains outstanding (§5). Cost: landed.

**Batch 17 — R05: lexical stats consistency** [correctness][owner]
Items: R05 [correctness][owner]. After fork §4.4 is answered. Slice (i):
fix the delete/purge asymmetry — `purge()` rewrites postings (stats go
live) while `delete()` does not, so the same logical operation changes
scores through one path and not the other. Slice (ii): live `N`/token
totals from per-segment live counters at tombstone time (no format
change). Slice (iii) — live per-term df — is the owner fork, not this
batch. Files: `fts/index.rs`, `ingest/purge.rs`, `lifecycle/mod.rs`.
VERIFY-NOW: scoring semantics. RED:
`delete_and_purge_of_the_same_rows_yield_identical_bm25_stats` — currently
RED by the verified asymmetry. Cost: M.

**Batch 18 — Graph scratch pool + deadline-aware wait** [perf]
Items: BL-127 [perf]. Verified: `graph_cache.rs:123-124` still parks every
concurrent same-segment query on a single condvar with no timeout, so
single-segment graph QPS is capped at 1 and a 1 ms-deadline query can wait
out a slow neighbour. Fix: scratch pool sized to the worker budget +
`wait_timeout` with a cancellation/deadline check per wakeup. **Every
scratch is an accounted ~1 MB allocation — `stats()` must be exact, which
is why this is VERIFY-NOW.** RED: two-thread test asserting concurrent
checkout proceeds, and a deadline-while-waiting test observing the typed
deadline error during the wait, not after; both currently RED. Files:
`lifecycle/graph_cache.rs`, `lifecycle/stats.rs`. Cost: M.

**Batch 19 — Segment-header hardening** [perf]
Items: BL-106 [perf]. Verified: `segment/reader.rs:1050` still does
`entries.iter().any(...)` per entry — O(n²) over up to 65,535
attacker-supplied regions (~2.1e9 comparisons behind a forgeable xxh3).
Fix: allocation-free bounded structure (stack bitmap over u16 kinds or
small sorted array); do NOT restore the HashSet (unaccounted allocation).
VERIFY-NOW: byte parser — corruption tests plus
`cargo fuzz run fuzz_smoke -- -max_total_time=60`. RED: existing
duplicate-kind corruption case must stay green; add a region-count
stress case with a wall-clock ceiling generous enough for zero flake.
Cost: S.

**Batch 20 — Delete the read-back that cannot fail** [test-gap][perf]
Items: BL-146 [test-gap], R08-cheap-slice [perf]. Verified: `publish_segment`
(`segment/writer.rs`, post-write `vfs.read` + compare) still re-reads every
published segment through the page cache before `data_file_sync` — it can
only ever catch the fault-VFS's injected truncation, and costs a full read
per seal/purge-rewrite/graph-publish. Delete it; state where the fault-VFS
case is covered. R08's streaming-writer rewrite is NOT this batch (§5).
VERIFY-NOW: publication path; crash matrix must stay green. RED: the fault
VFS truncation case must still be caught by whatever remains — prove by
running the crash matrix with the read-back deleted and the alternative
guard in place. Cost: S.

**>>> CHECKPOINT 3 — full workspace + fuzz smoke. <<<**

**Batch 21 — R12: derive fusion signals at the Store seam** [api]
Items: R12 [api]. After fork §4.6. Small adapter deriving `RuleSignals`
from the structured `TermQuery` inside `search_hybrid` (verified:
`fusion/mod.rs:146` passes `RuleSignals::none()` today, so shipped fusion
rules never fire from the Store path). Files: `lifecycle/mod.rs`,
`fusion/mod.rs`. VERIFY-NOW: hybrid ranking changes. RED:
`store_hybrid_populates_rule_signals_from_the_term_query` — currently RED.
Cost: S.

**Batch 22 — R04: bounded hybrid producers** [perf][api]
Items: R04 [perf][api]. BL-160's exact default and R15's fusion narration were
split out and implemented independently; this batch must preserve both.
Verified: `hybrid_vector_candidate_limit`
(`lifecycle/mod.rs:1408-1425`) sets `vector_k` to the total corpus row
count, and `exact_lexical_leg` requests `k = document_count` with forced
`AllowListDrive`, bypassing MAXSCORE/WAND; legs serial, fully
materialized. Design per the assessment: bounded producers, serial first,
score-scale contract at the seam defined against BL-134's fix (Batch 11)
so the producer does not rebuild that bug behind an abstraction. Requires:
Batch 6 baseline and Batch 11 landed. Files: `lifecycle/mod.rs`,
`fusion/mod.rs`, `planner/*` (5–10 files) + property tests. VERIFY-NOW: the hybrid query
path end to end. RED: counter test showing `vector_k = N` and forced
`AllowListDrive` today (the assessment's rebased RED evidence), then the
bounded-producer contract tests. Cost: L. **Largest hybrid-latency lever
in the backlog.**

**Batch 23 — Hygiene leftovers** [hygiene]
Items: BL-072 [hygiene][api] (narrow `KernelVariant::available()` or
feature-gate the bench accessor), BL-032 [hygiene][perf]
(`ExactCandidateStream::pull` still re-scans per pull — verified — but
also now has zero consumers outside its own module — verified — so the
right fix is probably deletion, not incrementalization), BL-002 [hygiene]
(fuzz-alias shim: keep parked, one-line note). DEFER-OK in nature but
scheduled here because BL-072/BL-032 touch public API surface: treat as
VERIFY-NOW-lite, validated by the package suite. RED: n/a (removals);
FFI header gate must stay green. Cost: S.

**Batch 24 — Coverage gates back to green** [test-gap]
Items: BL-155 [test-gap], BL-151 [test-gap], BL-111 [test-gap][perf];
closing BL-112 [owner] (its suspension is de-facto over — coverage.sh is
being run again and `tasks/evidence/P2.2-BL-112-coverage.md` exists; the
residual debt IS BL-155/BL-151). Work: cover workspace paths to ≥90%
(89.68% measured 2026-08-24), cover the bench frontier scope (86.91%), and
put the 1M-SIFT instrumented duplicate behind a `ZE_*` env var with a
same-lines small fixture (verify with a coverage diff; any lost line gets
a restoring test — never accept the drop). No threshold moves, no regex
widening. DEFER-OK code (test additions only) but scheduled LAST because
its validation is two full instrumented runs on a quiet machine, and it
depends on Batch 3's BL-156 fix to be runnable twice at all. RED: the
gates themselves are the standing RED (both measured failing on main).
Cost: M–L.

**>>> FINAL CHECKPOINT — the whole gate script. <<<**

---

## 2. Validation checkpoints

The in-flight suite run on the staged D6+D9 merge finishes and that merge
commits BEFORE any of this starts. Expect cargo-lock contention until then;
wait, don't work around it.

| checkpoint | after | command | why this command |
| --- | --- | --- | --- |
| CP1 | batches 1–8 | `cargo test --workspace --no-fail-fast`, then `scripts/coverage.sh` **twice back-to-back** (proves BL-156), one `cargo deny check` (proves Batch 2), `RUSTDOCFLAGS="-D warnings" cargo doc` (proves Batch 1) | the only stacked block; failures here are attributable because nothing engine-side moved |
| per-batch 9–22 | each VERIFY-NOW batch | `cargo test -p zeppelin-embed` + the named suites for the touched layer: crash matrix + `purge_proof` (9, 13, 20), `corruption` (10, 19), adversarial tests (11), memory-accounting suites (12, 18) | a later full run cannot attribute a silent break on these layers |
| CP2 | batch 12 | `cargo test --workspace --no-fail-fast` | before the maintenance rewrite stacks on the reader changes |
| CP3 | batch 20 | `cargo test --workspace --no-fail-fast` + `cargo fuzz run fuzz_smoke -- -max_total_time=60` | parser + publication path just changed |
| FINAL | batch 24 | `scripts/ci-gates.sh` end to end, in a clean shell | the gate script is the gate; a remembered subset is not (BL-153's lesson) |

`scripts/ci-gates.sh`, `cargo llvm-cov`, and `cargo deny` stay out of
routine per-batch verification; they appear above only where a batch's
subject IS that gate.

---

## 3. Dependency graph

```
BL-160 ruling (resolved; exact hybrid default) ─────────────────────► Task 22 ABI
B11 ──► re-read BL-159                             │
B6 (R13 baseline, numbers banked) ──► B10 ──► B12 ─┼─► B13
                                      │            │
                                      └────────────┴─► B22 (also needs B11)
B9 ──► B13   (welded pair: revert together)
B3 (BL-156) ──► B24 (coverage.sh must run twice)
B8 (perf floors) ──► B14, B15 (floors are their regression guard)
owner §4.4 ──► B17      owner §4.6 ──► B21      owner §4.5 ──► B16
B1..B8 mutually independent (B5 coordinates with staged I14 files)
B14, B15, B16, B17, B18, B19, B20, B23 independent of each other,
  each revertible alone; B22 conflicts textually with B11/B12 in
  lifecycle/mod.rs — rebase, don't reorder
campaigns: BL-142 ──► BL-149(=R09), BL-131 · B7 ──► BL-122, BL-089
  · B12 ──► re-profile BL-114
```

Welded (must revert as a unit): **B9+B13** (guard and rewrite), and
**B11+B22** once both have landed (the producer contract is defined
against B11's score-scale rule).

---

## 4. DO NOT FIX — owner decisions

1. **BL-160 [api][owner] — RESOLVED 2026-08-25.** Hybrid with no explicit
   tier selects exact scoring. `SearchOptions::with_tier` records an explicit
   choice, including explicit `Auto`, and that choice still wins. Fusion still
   rejects every estimated vector score with `EstimatedVectorScore`. Task 22
   consumes this ruling; R04 must preserve it while changing producer bounds.
2. **BL-100 [owner][perf] — the 19-M1 L2 target.** 2.034 ns/row vs an
   extrapolated 1.8: re-baseline is an owner HARD STOP; materiality note
   says the miss is 0.45% of the gate that matters. Rule at M4.
3. **BL-090 [owner][correctness] — zero-vector cosine semantics.** Policy,
   then an XS driver patch unlocks the missing nytimes cell.
4. **R05(iii) [correctness][owner] — live per-term df.** Exact live df
   costs per-term maintenance; the documented-approximation path must be
   explicit — fail-loudly forbids the quiet middle. Gates Batch 17.
5. **BL-008 [owner][api] — multi-valued fields.** "Single-valued only in
   v1" as an explicit ADR, or a LIST representation + predicate AST
   variants. Retrofit cost grows every task; silent omission is not an
   acceptable outcome per the entry.
6. **R12 [api][owner] — who may supply trusted `RuleSignals`.** Gates
   Batch 21.
7. **R06/§4.5 [correctness][owner] — is angular/normalized data a
   supported v1 metric surface?** Gates Batch 16's profile table.
8. **BL-104 [perf][owner] — raise QoS on the query path or not.** The
   diagnostics half already shipped (verified: `diag.rs` `observed_qos`,
   `graph/search.rs::observed_qos`, `qos-contention` bench). Remaining
   fork: silently raising a caller's QoS takes CPU from the host — that
   is a product call, not a fix.
9. **R14 [owner] — tokenizer conformance scope.** The intended
   language/script distribution is a product statement; the campaign
   waits on it.

---

## 5. NEEDS A MEASUREMENT CAMPAIGN, not a fix

Ordered by leverage. Every result lands in `tasks/evidence/` with hardware,
command, raw numbers; single-tenant discipline throughout (BL-089's rule).

1. **BL-142 [test-gap] P1** — vendor BEIR corpora fetch w/ checksums +
   commit the tantivy/FTS5 competitor harness. Unblocks the next two.
2. **BL-149 = R09 [perf] P1** — four-arm threshold experiment (pin ≤2 vs
   ≤3 on real TREC-COVID term distributions); extend to feature-based
   selection. Mechanism verified live (`select_strategy` ignores `k`).
3. **BL-131 [test-gap] P2** — BEIR gate into CI (mount or committed
   subset); verified still `#[ignore]`d behind `ZE_BEIR_DIR`.
4. **BL-122 [test-gap] P1** — recall-latency curves at matched recall for
   every competitor (ef/nprobe sweeps). Blocks any public claim. Needs
   Batch 7's load-gate fix first.
5. **BL-114 [perf] P1** — hop-loop instruction-overhead: **re-profile
   before executing** — two of its three cited mechanisms are already gone
   in the latency-trim refactor (verified: `hop_candidates` is a Vec;
   `decode_block_view` no longer exists). The rung-ladder falsifier stands.
6. **R07 [perf] P1** — maintenance checkpoint cadence: trace bytes/rows
   first (cadence 64 verified); the fix is a persistence policy, never
   just a bigger constant.
7. **BL-089 [test-gap] P2** — re-run the zeppelin cross-benchmark sweep
   single-tenant; nothing publishable until then.
8. **BL-082 [perf] P2** — the sustained-offered-load WAL sweep. The
   harness EXISTS now (verified: `bin/wal-throughput.rs`); only the swept
   runs + evidence are owed.
9. **BL-157 [test-gap] P2** — diagnostics assembly-cost control arm.
10. **BL-152 [perf] P2** — filtered-graph budget multiplier, fine
    selectivity sweep 1–10% (harness: `planner-thresholds`).
11. **R10 [hygiene] P2** — run/extend the Task 16 threshold harness to the
    fusion constants; provenance comments naming evidence files.
12. **BL-029 [test-gap] P2** — re-run quant recall on SIFT1M (now cached
    locally) / a real embedding dump.
13. **BL-048 [test-gap] P3** — re-measure Table 1 (kernel numbers moved
    unexplained) — fold BL-075's unconfirmed opt-z claim into the same
    session.
14. **BL-074 [perf] P2** — nibble-unpack elimination (1.3–1.5x one-core;
    measure at 12 threads before believing it matters).
15. **BL-071 [perf] P2** — batched-query Bit4 seam: design + measure
    (instruction-density win must convert to wall-clock).
16. **BL-041 [perf] P2** — bit4 single-core campaign toward its 41.4%
    roof (transpose-before-unpack).
17. **BL-061 [perf] P2** — scan latency during concurrent publish, then
    evaluate `F_NOCACHE_EXT`.
18. **BL-065 [perf] P3** — cold-first-query with `MADV_WILLNEED`
    (verified: still zero `madvise` anywhere in src).
19. **BL-064 [perf] P3** — extend platform-truth to 16 readers.
20. **BL-077 [test-gap] P2** — re-run the B2a directory-barrier ablation
    against a VFS that models entry durability (note: `vfs/crash.rs` now
    models rename-persisted/content-not states — the modeling gap has
    narrowed since filing; the ablation itself never re-ran).
21. **BL-026 [test-gap] P1 + BL-015 [test-gap] P1 + R11 [perf] P2** —
    frontier-loop governance: the `pmu-capture` operator-artifact
    subcommand (capture works unsandboxed; slot attribution still
    unmapped), pluggable real workloads, and the worker-budget experiments
    — all pre-conditions for any future 27-B campaign, none of them fixes.

---

## 6. PROPOSED CLOSE — no work needed, with evidence

Verified fixed, subsumed, or unreachable. Confidence high unless flagged.

| item | tags | evidence |
| --- | --- | --- |
| BL-153 (P1) | [hygiene] | `cargo fmt --all -- --check` exits 0 on current main (run 2026-08-25). Residue (run the script, not a subset) is a recorded lesson, not work. |
| BL-139 (P1) | [hygiene] | Same fmt run; Track L's files merged and are formatted. Subsumed by BL-153's history. |
| BL-154 (P1) | [hygiene] | Commit `74beb8d` "Fix the sixteen doc links the rustdoc gate rejects" — matches the entry's 16-link inventory, including the public→private class. |
| BL-141 (P1) | [perf] | `Cargo.toml` `[profile.release] opt-level = 3` with the O6 owner-decision comment (2026-08-24); CLAUDE.md records the measured basis. Residual A/B on the competitive claim folds into campaign #1/#4. |
| BL-012 (P1) | [perf] | `kernels/neon.rs:607-640`: four independent `vfmaq_f32` accumulators, single horizontal sum. Task-03 close log records 432.7→43.3 ns (f32), 888.6→44.3 (f16). |
| BL-011 (P2) | [test-gap] | Task-03 close log: cold-DRAM bench at 1.536 GB added in fix round 1; BL-013's decision consumed its output. |
| BL-013 (P1) | [owner] | Decision executed: `tasks/evidence/02-platform-truth.md:263-274` carries the wide-load 80.689179 comparison; 27-H gates against it (BL-018 numbers). |
| BL-014 (P1) | [owner] | 27-H close log: `CampaignStop::Complete` unconstructible without irreducible `PmuReport` (type-level, verified by the orchestrator at the time); targets-as-tripwires written into the stop rule. |
| BL-018 (P1) | [test-gap] | `tasks/evidence/27-h-harness.md:79-87` carries the three GMAC/s denominators with raw medians and checksums — the exact numbers the entry demanded. |
| BL-017 (P2) | [test-gap] | `fuzz/seeds/` exists with four target directories + README; task-04 log confirms seed-corpus injection landed. |
| BL-069 (P1) | [test-gap] | `benches/scan.rs`: `--fixture degenerate\|clustered\|random` honored per scheme (`build_int8` branches on FixtureKind, :404-434), random is the default. The instrument defect is gone; historical best-case numbers stay flagged via Batch 1 pointers. |
| BL-039 (P3) | [test-gap] | Same rewrite: F16 is a full scheme arm (:18,:49,:103-105), no `min(256)` clamp remains, fixture generators exist. |
| BL-099 (P2) | [test-gap] | `bin/gather-kernel.rs`: `WARMUP_PASSES` + warmup checksum + median reporting + `build_profile=bench` context line — every element of the dispatched fix. |
| BL-095 (P1) | [test-gap] | `bench/src/platform/memory_graph.rs:1233-1258`: `address_serialized_next_block_derives_from_current_block_contents` now runs the REAL `gather_serialized` and asserts a mutated block changes the next address. |
| BL-105 (P2) | [test-gap] | `tests/snapshot_remap.rs:823-849`: touch-then-assert residency rises ≥80% of mapped on the same mapping, and the fixture forces the 1024-page chunked mincore path — the delta test the entry's REAL FIX demanded; a constant-return implementation cannot show a rise. |
| BL-108 (P2) | [test-gap] | `tests/close_contract.rs:39-47,239`: exact `mapped_bytes` equality and `resident_owned_bytes == 0` after close — the deterministic counter gate beside the footprint one. |
| BL-109 (P3) | [test-gap] | `#[repr(C, packed(4))]` present in both `close_contract.rs:355` and `snapshot_remap.rs:1001`. |
| BL-096 (P2) | [correctness] | Scan loops now check cancellation every 64 rows (`scan_squared_l2`, verified) and `close_contract.rs:162,266` prove drain/cancel and lease-outlives-store behavior — 09-D's obligation, delivered. |
| BL-118 (P2) | [test-gap] | `graph/search.rs:2096` `traversal_expands_the_nearest_frontier_node_before_insertion_order` + `chain_graph_fixture`/`long_chain_graph_fixture`/`best_first_graph_fixture` (:2463,:2561,:2501) — the sparse order-sensitive fixtures the entry demanded, in-crate. |
| BL-126 (P3) | [test-gap] | `tests/store_graph_search.rs:37-38`: `BEST_FIRST_ROWS=145`, `EF=140` (ef < node_count) with a frontier-order-decisive topology (`publish_best_first_auto_graph`) — the store-level half, separately from BL-118. |
| BL-132 (P1) | [perf] | `fts/sealed.rs` module header: "the persisted format, on the query path"; `:1228` names itself the production caller of `kernels::postings::unpack`. The BTreeMap-only query path is gone. |
| BL-130 (P2) | [perf] | Marked superseded by BL-132 in its own header; sealed streams carry `block_bound`/impact pairs on the query path. Closes with BL-132. |
| BL-036 (P2) | [perf] | Early abandonment DELETED — `tasks/execution_order.md:178`: "commit cb91a85, ADR-003 … never won a single measurement." No `abandon` in `scan/`. |
| BL-037 (P1) | [perf] | The PDX scan arms are gone from `scan/mod.rs` (zero `Pdx` hits; survives only in `segment/layout.rs` as format vocabulary). The decode-then-row-major pessimization no longer exists to fix. |
| BL-045 (P3) | [perf] | Same deletion — with one scan layout shipping, thread-budget layout selection is moot. Any revival belongs to Task 20's ladder, as the entry itself said. |
| BL-042 (P2) | [test-gap] | All four harness fixes recorded as applied in-entry (per-reader denominator table + >100% guard, iteration scaling, build-profile assertion, core-residency canary); (c) is a standing caveat, not work. |
| BL-092 (P2) | [test-gap] | `tasks/cross-benchmark/harness/src/drivers/zeppelin.rs:300-355`: driver now encodes bit4 rows, labels `coarse_query_quantization: bit4`, queries via `prepare_bit4_query`/`rescore_top_k` — the bit4 read path is what is measured. |
| BL-150 (P2) | [test-gap] | `bench/src/graph_recall.rs:189,336,386,690`: `peak_rss_bytes` is measured at build, written into the cache marker, and parsed back on reuse. |
| BL-147 (P1) | [correctness] | Premise re-verified live — closed as a DUPLICATE subsumed by R01 (Batches 9+13), per the assessment. Not fixed yet; the work survives under R01. |
| BL-148 (P1) | [correctness] | Same: subsumed by R01, Batch 13 carries the unlink. |
| BL-112 (P1) | [owner] | The suspension has ended de facto: coverage.sh is being run again (BL-155/BL-151 are its measurements) and `tasks/evidence/P2.2-BL-112-coverage.md` exists. The residual debt IS BL-155+BL-151 → Batch 24. |
| BL-102 (P1) | [test-gap] | **Medium confidence.** The fault runner now includes graph operations (`tests/adversarial/program.rs:14` `SearchKind::Graph`; runner drives `SearchTier::Graph` and checks graph plans/regions), and the staged I14 work extends it. Close condition (task-19 components in the runner) appears met; confirm by reading the adversarial suite's op coverage once the staged merge commits, before striking the entry. |
| BL-031 (P2) | [test-gap] | **Medium confidence.** Process item; the mutation-based mitigation is recorded in-entry, the dispatch-prompt rule lives in the lessons file, and the entry's own body carries two RESOLVED sub-notes. No code deliverable remains. |

Also closing-by-motion: **BL-159** (recorded bound, no work — retire when
Batch 11 lands and I13 can see the planner again). **BL-160 is implemented**
independently of R04, with RED/GREEN and mutation guards in
`store_text_columns.rs`.

---

## 7. Full item table

Columns: item · tags · priority · disposition · blast radius · cost ·
verification pass. B*n* = batch; CLOSE = §6; CAMP = §5; OWNER = §4.

| item | tags | P | disposition | blast radius | cost | pass |
| --- | --- | --- | --- | --- | --- | --- |
| BL-001 | [test-gap] | P2 | B8 | scripts/size-budget.sh, consumer bin | S | 1 |
| BL-002 | [hygiene] | P3 | B23 (park note) | .cargo/config.toml, tools/fuzz-dispatch | XS | 2 (alias verified) |
| BL-003 | [test-gap] | P3 | B3 | size-budget test | XS | 1 |
| BL-008 | [owner][api] | P2 | OWNER §4.5 | columns AST, planner, format-additive | L | 1 |
| BL-011 | [test-gap] | P2 | CLOSE | — | — | 2 (log+evidence) |
| BL-012 | [perf] | P1 | CLOSE | — | — | 2 (neon.rs read) |
| BL-013 | [owner] | P1 | CLOSE | — | — | 2 (evidence read) |
| BL-014 | [owner] | P1 | CLOSE | — | — | 2 (27-H log) |
| BL-015 | [test-gap] | P1 | CAMP #21 | frontier harness design | L | 1 |
| BL-017 | [test-gap] | P2 | CLOSE | — | — | 2 (fuzz/seeds ls) |
| BL-018 | [test-gap] | P1 | CLOSE | — | — | 2 (evidence read) |
| BL-023 | [test-gap] | P1 | B8 | scripts/ci-gates.sh, bench floors | M | 1 |
| BL-025 | [test-gap] | P1 | B7 | bench measure.rs preflight | S | 1 |
| BL-026 | [test-gap] | P1 | CAMP #21 | frontier/pmu.rs, operator artifact | M | 1 |
| BL-029 | [test-gap] | P2 | CAMP #12 | recall harness runs | M | 1 |
| BL-031 | [test-gap] | P2 | CLOSE (med conf) | — | — | 1 (entry text) |
| BL-032 | [hygiene][perf] | P2 | B23 | scan/mod.rs (delete stream) | S | 2 (pull + zero consumers) |
| BL-033 | [hygiene] | P3 | B1 | evidence-file process note | XS | 1 |
| BL-036 | [perf] | P2 | CLOSE | — | — | 2 (deletion verified) |
| BL-037 | [perf] | P1 | CLOSE | — | — | 2 (no Pdx in scan) |
| BL-039 | [test-gap] | P3 | CLOSE | — | — | 2 (scan.rs read) |
| BL-041 | [perf] | P2 | CAMP #16 | bit4 kernel campaign | L | 1 |
| BL-042 | [test-gap] | P2 | CLOSE | — | — | 1 (entry: fixes recorded) |
| BL-045 | [perf] | P3 | CLOSE | — | — | 2 (deletion verified) |
| BL-048 | [test-gap] | P3 | CAMP #13 | Table 1 re-measure | S | 1 |
| BL-049 | [hygiene] | P2 | B1 | scripts/coverage.sh comment | XS | 1 |
| BL-061 | [perf] | P2 | CAMP #17 | vfs write path, F_NOCACHE_EXT | M | 1 |
| BL-064 | [perf] | P3 | CAMP #19 | platform-truth sweep | XS | 1 |
| BL-065 | [perf] | P3 | CAMP #18 | segment/reader.rs mmap open | S | 2 (zero madvise) |
| BL-069 | [test-gap] | P1 | CLOSE | — | — | 2 (scan.rs read) |
| BL-070 | [test-gap] | P1 | B7 | b1-measure, scheme-level, opt-ledger | S | 2-partial |
| BL-071 | [perf] | P2 | CAMP #15 | scan seam API + kernel | M | 1 |
| BL-072 | [hygiene][api] | P3 | B23 | kernels dispatch surface | S | 1 |
| BL-073 | [perf] | P2 | B15 | kernels dispatch, BoundedTopK | S–M | 1 |
| BL-074 | [perf] | P2 | CAMP #14 | bit4 kernel | M | 1 |
| BL-075 | [test-gap] | P2 | CAMP #13 | evidence only | S | 1 |
| BL-077 | [test-gap] | P2 | CAMP #20 | vfs test model + ablation | M | 2-partial (crash.rs) |
| BL-082 | [perf] | P2 | CAMP #8 | wal-throughput runs | M | 2 (harness exists) |
| BL-084 | [hygiene] | P3 | B1 | evidence cross-refs | XS | 1 |
| BL-085 | [hygiene] | P3 | B1 | execution_order.md:290 | XS | 2 (line verified) |
| BL-089 | [test-gap] | P2 | CAMP #7 | cross-benchmark re-run | M | 1 |
| BL-090 | [owner][correctness] | P2 | OWNER §4.3 | driver + engine cosine contract | XS after | 1 |
| BL-091 | [hygiene] | P1 | B1 | tasks/19,20 text | XS | 2-partial (10k in code) |
| BL-092 | [test-gap] | P2 | CLOSE | — | — | 2 (driver read) |
| BL-094 | [hygiene] | P2 | B1 | root CLAUDE.md Quickstart | XS | 2 (in context) |
| BL-095 | [test-gap] | P1 | CLOSE | — | — | 2 (test read) |
| BL-096 | [correctness] | P2 | CLOSE | — | — | 2 (loop + tests) |
| BL-099 | [test-gap] | P2 | CLOSE | — | — | 2 (bin read) |
| BL-100 | [owner][perf] | P2 | OWNER §4.2 | 19-M1 target | — | 1 |
| BL-101 | [test-gap] | P3 | B7 | recurring prefetch pair | S | 2-partial (arms exist) |
| BL-102 | [test-gap] | P1 | CLOSE (med conf) | — | — | 2-partial (program.rs) |
| BL-103 | [test-gap] | P2 | B7 | DRAM-gate protocol + memory_graph audit | S | 1 |
| BL-104 | [perf][owner] | P1 | OWNER §4.8 | QoS policy; diag half shipped | — | 2 (diag verified) |
| BL-105 | [test-gap] | P2 | CLOSE | — | — | 2 (test read) |
| BL-106 | [perf] | P3 | B19 | segment/reader.rs:1050 | S | 2 |
| BL-108 | [test-gap] | P2 | CLOSE | — | — | 2 (test read) |
| BL-109 | [test-gap] | P3 | CLOSE | — | — | 2 (both files) |
| BL-111 | [test-gap][perf] | P2 | B24 | coverage.sh + env-gated gate | S | 1 |
| BL-112 | [owner] | P1 | CLOSE → B24 | — | — | 1 (evidence file exists) |
| BL-114 | [perf] | P1 | CAMP #5 (re-profile) | graph/search.rs hop loop | L | 2 (mechanisms moved) |
| BL-115 | [perf] | P2 | B14 | quant/rescore.rs:358 | M | 2 (relocated, verified) |
| BL-118 | [test-gap] | P2 | CLOSE | — | — | 2 (tests read) |
| BL-119 | [test-gap] | P2 | B7 | bin/graph-search.rs:266 | S | 2 |
| BL-120 | [test-gap] | P2 | B4 | graph/search.rs tests | S | 2 |
| BL-121 | [hygiene] | P3 | B1 | kernels/mod.rs:230-263 | XS | 2 |
| BL-122 | [test-gap] | P1 | CAMP #4 | cross-benchmark sweeps | L | 1 |
| BL-125 | [test-gap] | P1 | B7 | run_matrix.py load gate | S | 1 |
| BL-126 | [test-gap] | P3 | CLOSE | — | — | 2 (fixture read) |
| BL-127 | [perf] | P2 | B18 | graph_cache.rs, stats | M | 2 |
| BL-128 | [perf] | P2 | B14 | lifecycle scan_squared_l2 | M | 2 |
| BL-130 | [perf] | P2 | CLOSE | — | — | 2 (via BL-132) |
| BL-131 | [test-gap] | P2 | CAMP #3 | beir_gate.rs, CI | M | 2 (ignore verified) |
| BL-132 | [perf] | P1 | CLOSE | — | — | 2 (sealed.rs read) |
| BL-134 | [correctness][api] | P0 | B11 | lifecycle, planner (query-side only) | M–L | 2 |
| BL-137 | [test-gap] | P1 | B5 (verify+re-run) | adversarial runner | S | 2-partial |
| BL-139 | [hygiene] | P1 | CLOSE | — | — | 2 (fmt run) |
| BL-140 | [hygiene] | P1 | B2 | deny.toml | XS | 2 |
| BL-141 | [perf] | P1 | CLOSE | — | — | 2 (Cargo.toml) |
| BL-142 | [test-gap] | P1 | CAMP #1 | corpus fetch + competitor harness | L | 1 |
| BL-146 | [test-gap][perf] | P2 | B20 | segment/writer.rs publish | S | 2 |
| BL-147 | [correctness] | P1 | CLOSE (dup → R01) | — | — | 2 |
| BL-148 | [correctness] | P1 | CLOSE (dup → R01) | — | — | 2 |
| BL-149 | [perf] | P1 | CAMP #2 (=R09) | fts/prune selector + evidence | M | 2 (selector read) |
| BL-150 | [test-gap] | P2 | CLOSE | — | — | 2 (marker read) |
| BL-151 | [test-gap] | P1 | B24 | bench-crate tests | M | 1 (recent measurement) |
| BL-152 | [perf] | P2 | CAMP #10 | planner budget constant | M | 1 |
| BL-153 | [hygiene] | P1 | CLOSE | — | — | 2 (fmt run) |
| BL-154 | [hygiene] | P1 | CLOSE | — | — | 2 (commit) |
| BL-155 | [test-gap] | P1 | B24 | core-crate tests | M | 1 (recent measurement) |
| BL-156 | [test-gap] | P1 | B3 | ffi_header.rs | S | 2 |
| BL-157 | [test-gap] | P2 | CAMP #9 | diag assembly control arm | S | 1 |
| BL-158 | [test-gap] | P2 | B5 | adversarial runner + trace source | M | 2 |
| BL-159 | [test-gap] | P3 | re-read after B11 | — | — | 1 |
| BL-160 | [api][owner] | P2 | LANDED (`api/bl160-r15`) | hybrid default, C ABI shape | S | RED/GREEN + mutation guards |
| BL-161 | [test-gap][format] | P2 | B3 | tests/corruption.rs | S | 2-partial |
| R01 | [correctness] | P0 | B9 + B13 | graph/build, segment/writer, tier/maintain | S then L | 2 (re-verified) |
| R02 | [perf] | P0 | B10 | segment/reader.rs | S–M | 2 (re-verified) |
| R03 | [perf] | P0 | B12 | reader, fts/sealed, planner, lifecycle | L | 2-proxy (assessment) + spot |
| R04 | [perf][api] | P1 | B22 | lifecycle hybrid seam, fusion | L | 2 (vector_k re-verified) |
| R05 | [correctness][owner] | P1 | B17 + OWNER §4.4 | fts/index, purge | M | 2-proxy (assessment) |
| R06 | [correctness] | P1 | B16 | profiles, maintain, planner | M | 2 (sift_1m re-verified) |
| R07 | [perf] | P1 | CAMP #6 | tier/maintain checkpoint policy | M | 2 (cadence re-verified) |
| R08 | [perf] | P2 | B20 (cheap) + deferred rewrite | segment/writer | S / L | 2 (read-back re-verified) |
| R09 | [perf] | P1 | CAMP #2 (dup of BL-149) | — | — | 2 |
| R10 | [hygiene] | P2 | CAMP #11 | threshold harness + provenance | M | 2-proxy |
| R11 | [perf] | P2 | CAMP #21 | worker budget experiments | L | 2-proxy |
| R12 | [api] | P2 | B21 + OWNER §4.6 | fusion signals adapter | S | 2 (none() re-verified) |
| R13 | [test-gap] | P1 | B6 | new bench bin | M | 2-proxy |
| R14 | [owner] | P2 | OWNER §4.9 | tokenizer campaign | L | 2-proxy |
| R15 | [hygiene] | P2 | B1 (+B22 residue) | 4 doc sites | XS | 2-proxy (assessment all four) |

---

## 8. Coverage statement

**Opened source / ran a check in this pass (pass 2):** BL-002, BL-011,
BL-012, BL-013, BL-014, BL-017, BL-018, BL-032, BL-036, BL-037, BL-039,
BL-045, BL-065, BL-069, BL-070 (partial), BL-077 (partial), BL-082, BL-085,
BL-091 (partial), BL-092, BL-094, BL-095, BL-096, BL-099, BL-101 (partial),
BL-102 (partial), BL-104 (diag half), BL-105, BL-106, BL-108, BL-109,
BL-114 (mechanisms), BL-115, BL-118, BL-119, BL-120, BL-121, BL-126,
BL-127, BL-128, BL-130, BL-131, BL-132, BL-134, BL-137 (partial), BL-139,
BL-140, BL-141, BL-146, BL-147, BL-148, BL-149 (selector), BL-150, BL-153,
BL-154, BL-156, BL-158, BL-161 (partial); R01, R02, R04, R06, R07, R09,
R12 re-verified directly; R03, R05, R08, R10, R11, R13, R14, R15 accepted
from the same-day assessment (which opened the cited lines) with spot
checks where noted.

**Classified from entry text + at most one targeted grep (pass 1):**
BL-001, BL-003, BL-008, BL-015, BL-023, BL-025, BL-026, BL-029, BL-031,
BL-033, BL-041, BL-042, BL-048, BL-049, BL-061, BL-064, BL-071, BL-072,
BL-073, BL-074, BL-075, BL-084, BL-089, BL-090, BL-100, BL-103, BL-111,
BL-112, BL-122, BL-125, BL-142, BL-151, BL-152, BL-155, BL-157, BL-159,
BL-160. Of these, BL-151/BL-155 rest on measurements taken 2026-08-24/25
by the orchestrator; BL-160 rests on the same-day assessment; the rest are
bench/campaign/owner items where the entry text is the artifact.

**Not run in this pass:** any coverage command, any deny check beyond
reading `deny.toml`, any benchmark, and anything needing the cargo build
lock (a full suite was running against the staged merge throughout; the
one `cargo fmt --check` used no build lock). Consequence: the two red
coverage gates (BL-155, BL-151) are taken on the entries' recent
measurements, not re-measured here.
