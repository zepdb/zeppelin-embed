# ZE-144 sparse preparation readiness

Verdict: **NO-GO for a new parallel sparse-preparation implementation ticket at this revision.** Keep ZE-61's prerequisites and acceptance unchanged. Do not dispatch a Sol executor for this proposed split.

Inspected main: `439237bd382e6186cf6ba869acf0d817284b8937`, 2026-09-20. ZE-144 was claimed before source inspection and remains `in_progress` for root review. This is a source audit; no product edit, build, test, worktree, or new ticket was made. Operational limits for any later authorized executor are `/tmp/graph-sol-executor-rules.md`.

## Why the existing producers are insufficient for this split

The real canonical producers and admitted preparation ownership exist. What is absent is the **native sparse search state and its semantic participant boundary**: checked source-local row-to-full-NodeId/version mappings, actual live membership, and text/vector active/checkpoint state interpreted against that same base. Canonical payload availability does not supply this state.

All source paths below are relative to `/Users/aghatage/Documents/code/zeppelin-embed`; line numbers are from the inspected revision.

| Existing source | What it establishes; remaining boundary |
| --- | --- |
| `src/property_graph/staging.rs:330-394` (under `crates/zeppelin-embed/`) | `StagedBatch` supplies normalized changed entities, canonical bytes, provenance, and base identity; replay rows are excluded. It does not supply source-local index rows or postings. |
| `src/property_graph/staging/encode.rs:217-224` | The staged after-membership sets `text: self.text.is_some()`. It is **not T**: empty and analyzed-empty text must remain outside lexical membership. Copying these flags into a new index participant would be incorrect. |
| `src/property_graph/catalog/interpretation.rs:103-163` | The compiled catalog supplies lexical interpretation and zero/one document space, with exact compatibility checks. It does not supply derived membership state. |
| `src/property_graph/storage/records/native.rs:51-85`; `src/property_graph/storage/view.rs:247-275` | Real records provide checked revision and original optional text/vector payloads. These are sufficient canonical read inputs, not existing sparse index membership. |
| `src/property_graph/storage/view/preparation_source.rs:67-119,247-332` | Real preparation has an authenticated lease, authentic nested memory owner, immutable source and validated catalog. This removes the earlier source/ownership gap. |
| `src/property_graph/storage/adjacency/prepare.rs:31-94,181-195` | The real native candidate owns directories/OUT/IN roots. Its vector/text fields are explicitly **expected admitted** descriptors; they are copied unchanged from `base.committed`, not newly prepared retrieval roots. |
| `src/property_graph/storage/view/prepared.rs:125-156,160-205,299-332` | `GraphPreparation` prepares native graph artifacts, finishes its object packs and returns the registered native candidate. Its result has no search delta or prepared search participant slot. Extending this complete handoff is a shared writes/storage/retrieval contract change, not an existing sparse producer to consume. |
| `src/property_graph/wal/replay.rs:4-19,31-75` | `RetrievalState = 6` and mandatory semantic validator hooks exist. A role tag and hook are not a sparse checkpoint codec/validator or an implementation of replay membership. |
| `src/ingest/active.rs:662-717`; `src/fts/index.rs:394-412` | Existing ingestion requires vector rows and backfills empty lexical rows. `SegmentIndex::push_document` finishes a row even with zero tokens. Reusing these paths unchanged would violate graph optional membership and lexical N. |

The normative boundary is explicit: retrieval owns derived indexes/mappings, storage owns canonical payloads, and writes owns one publication (`docs/graph/plans/retrieval.md:11-15`). T excludes absent, empty and analyzed-empty text; row identity includes graph content version; replacement/delete membership changes are atomic (`:45-53`). A correct preparation must fit the complete private search participant in the single prepared commit (`docs/graph/plans/writes.md:11-31,51-60`), rather than publish an independent ingest state. Storage preparation by itself remains the native-artifact participant (`docs/graph/plans/storage.md:19-26`).

A from-empty token/vector collector could be implemented now, as could a generic callback around `GraphPreparation`. Neither completes the requested boundary: replacement/deletion needs a defined old sparse state, and checkpoint/recovery needs a defined interpreted representation. Re-analyzing canonical text could determine whether a node belongs to T, but does not establish or validate its old derived row/version. Selecting all those formats and handoffs now would be new shared architecture; delegating only a wrapper would manufacture an incomplete component. No missing query-adapter implementation is being copied or guessed here.

## Scheduling and ownership

- **New implementation ticket:** none proposed. ZE-61 remains `todo` and blocked by ZE-60 (its complete recorded prerequisite list is ZE-29/35/42/60). Do not start it under ZE-144.
- **Product file ownership:** empty. This audit owns only `/tmp/ze-sparse-preparation-parallel-plan.md`. No registration edit, kernel refactor, fixture change or retrieval stub is needed.
- ZE-60's planner owns the real view/identity/payload adapter and its retrieval module registration. It explicitly does not own `storage/view/prepared.rs` or implement sparse participants. Its planned outputs are not yet compiled prerequisites for another worker.
- Revisit after ZE-60 is compiled and integrated. Plan ZE-61 against that fixed adapter together with its sparse active/checkpoint representation and actual prepared-participant handoff. If root wants an earlier producer split, it first needs an explicit, single-owner shared format/handoff design and acceptance mapping; this report does not authorize one.

## Verification boundary for the next review

There is no implementation RED/GREEN sequence to run for this no-go. Existing tests `native_preparation_coordinator_owns_admitted_source_and_finalization`, `native_prepared_artifacts_retain_exact_base_source_and_abort_owners`, and `native_prepared_artifacts_reject_foreign_or_stale_base` verify the current native preparation boundary; they are not sparse-membership evidence and were not rerun.

ZE-61 must still name and observe focused RED/GREEN cases for graph-only/empty/analyzed-empty exclusion from T; text-only/vector-only/both membership; full-width row identity/version checks; atomic replacement/deletion; checkpoint corruption; and active/sealed/reopened/compacted/older-retained-view equivalence (`docs/graph/plans/retrieval.md:94-101`). No tests or interfaces with invented names are prescribed before that actual design exists. Its relevant compile-only matrix must include core default, core `graph-cypher`, and `zeppelin-embed-workspace-tests` with default/`graph-cypher`/`graph-result-test-support`; compile the runner only.

ZE-118 retains final workspace, adversarial campaign, coverage, size, dependency-policy, sanitizer/platform/release qualification. This deferral does **not** move ZE-61's component or original lifecycle integration acceptance to ZE-118, and does not turn controlled installation into durable publication/recovery proof. Any eventual executor stops and escalates to root after two attempted fixes of the same failure, as required by `/tmp/graph-sol-executor-rules.md`; it does not broaden fixtures, budgets or interfaces to force GREEN.
