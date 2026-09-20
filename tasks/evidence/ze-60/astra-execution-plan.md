# ZE-60: view-bound search preparation and identity adapters

Executor: GPT-5.6-Sol / xhigh. Base: `439237bd382e6186cf6ba869acf0d817284b8937`.
Worktree: `/Users/aghatage/Documents/code/zeppelin-embed-wt-ze-60`, branch
`codex/ze-60-view-search-adapters`. ZE-60 is already claimed in_progress.
Read `/tmp/graph-sol-executor-rules.md`; its bounded execution rules apply.
Preserve `/tmp/ze-60-preservation.json`. No other ticket claim or subagents.

## Acceptance boundary

Complete ZE-60's original private read-adapter requirement: a real native-view
search context, reusable source-independent preparation/materialization leaves,
full-width NodeId/index identity conversion, same-view eligibility validation,
source-bound payload access, and actual native lease/resource cleanup. Distinct
IDs sharing their low 64 bits must remain distinct. A membership identity that
resolves to a missing, tombstoned, or differently versioned native record must
fail loudly. Borrowed records/text/vector bytes must not escape admission.

This implements preparation of the view, request and identity/payload access.
It does not claim actual index statistics, membership population, ranking, or
public query execution. Those are ZE-61/62/63/64/65. The accepted retrieval plan
already assigns sparse population and ranking separately. Root reviewed the
concrete shared preparation leaf below; do not silently reduce this to an ID
conversion utility or copy legacy snapshot-bound caches.

Canonical contracts are in the main checkout's AGENTS.md,
`docs/graph/plans/retrieval.md` and `parallel-contracts.md`. ZE-136 and completed
ZE-138 establish the actual typed search intent; they do not provide a ranker.
This planning pass ran no builds/tests and changed no product files.

## Actual compiled seams to use

| Source | Relevant existing interface |
| --- | --- |
| `property_graph/storage/view.rs` | Private `GraphReadView<'s,'lease,'m,'g>` owns no independent admission. It borrows the actual lease/source/catalog. `lookup_node`, `stored_text`, `vector_payload` return source-bound verified access. |
| `lifecycle/native_graph.rs` | `NativeReadConsumer::consume(view, runtime)` and `Store::with_native_read` provide the real scoped admission, source, catalog and query memory. `NativeReadLease` implements `RetainedView`. |
| `storage/view/source.rs` | `NativeQuerySource` binds exact lease, QueryMemory pointer and RuntimeInstanceId. Its `lease()`, `memory()`, `runtime()` are available to sibling view code. |
| `query/eligibility.rs` | Existing `Eligibility::{AllIndexed, Set}`; `EligibleNodeSet::ids_for(&QueryView)` checks exact pointer identity, not equal generation. Borrow it; never build/charge a second set. |
| `query/runtime.rs`, `query/resources.rs` | Same `RuntimeContext`, `QueryMemory`, `QueryArena`, reservations, close-first checkpoint and cumulative counters. `TreeResources::for_query` borrows this runtime. |
| `ingest/mod.rs`, `property_graph/identity.rs` | `DocId(u128)`, `DocumentVersion(DocId, Revision(u64))`; `NodeId(u128)` and positive `GraphRevision(u64)`. Explicit private conversion only, no public `From<NodeId>` implementation. |
| `storage/records/native.rs`, `records.rs` | `RecordView::incarnation/revision/canonical/provenance`; canonical optional text and original `StoredVector` preserve exact bits. |
| `staging.rs` | Authentic `StagedBatch::base/deltas`, `NormalizedDelta::provenance/canonical/shape/membership`. Same identity conversion must accept real node deltas; relationships are not search rows. `membership.text` is not proof of analyzed nonempty T. |
| `lifecycle/mod.rs::search_pinned` (~5975) | Existing source-independent empty/maximum/finite f32 validation precedes `PreparedVectorQuery::validate_binding`. Extract this leaf, not admission or score caches. |
| `lifecycle/materialize.rs::QueryMaterializer::materialize_address` | Existing expected-versus-actual `DocumentVersion` equality before text copying. Extract this small check for both legacy and native callers. |
| `query/plan/search.rs` | Landed `SearchMode::{Default, Auto, Exact, Scan}`, `SearchBounds`; request intent is not actual coverage/report evidence. Reuse; do not change the IR. |

`PreparedVectorQuery` and `PreparedLexicalQuery` retain legacy Accounting,
PublishedSnapshot/LexicalAssembly/segment state. They cannot be made native by
passing a new generation number. No native sparse LexicalIndex/vector producer
exists yet; do not manufacture one for this ticket.

## File ownership

Own new `crates/zeppelin-embed/src/property_graph/retrieval.rs` and small child
files beneath `property_graph/retrieval/` only as needed. Register it privately
in `property_graph/mod.rs`. Put GraphReadView additions in
`property_graph/storage/view/retrieval.rs`, with one additive `mod retrieval;`
in `storage/view.rs`. Do not expose lease/source construction publicly.

Own narrow extraction changes in `lifecycle/prepared.rs`,
`lifecycle/materialize.rs`, and only the corresponding call/reexport changes in
`lifecycle/mod.rs`. Put actual-producer tests in a new child
`lifecycle/native_graph/tests/retrieval.rs`; add `mod retrieval;` inside its
existing test module to access the existing private fixtures. Production
`lifecycle/native_graph.rs`, publication, GraphPreparation and storage codecs
are unchanged. Evidence lives in `tasks/evidence/ze-60/`.

Root coordinates two shared additive module lines with other workers. The
expression worker may own `storage/view/expression.rs` and catalog symbolic
access. ZE-60 does not edit `storage/view/catalog.rs` or `view/prepared.rs`.
Any wider file change needs a precise explanation to root first.

## Ordered implementation

1. Add the named RED tests below before their corresponding implementation.
   Reuse the small actual `stage_structured` -> `prepare_native_graph` artifact
   fixture and controlled installer already in native_graph tests. No mock
   RetainedView, fake successful lookup, dummy optional search participant or
   hand-encoded native record may stand for integration acceptance.

2. Add a small private `GraphReadView` validation/access leaf in its retrieval
   child. Check its real lease for close first, then runtime checkpoint, then
   exact QueryView pointer, source QueryMemory pointer and RuntimeInstanceId.
   Return borrowed view identity and `GraphInterpretation` reconstructed from
   that lease's actual lexical/document declaration. This exposes no lease
   constructor or publication authority. Reject mismatches before any new
   lookup/map/counter work. Do not trust store/generation equality alone.

3. Implement private explicit conversion of `(NodeId, GraphRevision)` to
   `DocumentVersion` and checked inverse (reject zero ID/revision). Use all 128
   ID bits and all 64 revision bits. Reuse this conversion for actual staged
   node-delta identity and read record identity; do not infer analyzed text
   membership, allocate physical row mappings, or reinterpret relationships.

4. Extract the source-independent validation pass from `search_pinned` into
   `lifecycle/prepared.rs`: nonempty, caller-supplied maximum geometry, finite
   coordinates, and a fallible per-coordinate control callback. Preserve exact
   legacy QuantError variants/order and its existing 65,536 limit at the legacy
   caller. A callback-control error must remain typed and distinguishable from
   invalid vector data. Do not allocate quantized buffers or redesign the cache.
   Native preparation calls the same leaf using the admitted document geometry,
   its existing graph input/list limits, and the authentic RuntimeContext.
   Do not impose the legacy quantized 65,536 limit on native Exact preparation.
   Count actual coordinate/byte visits using VectorCoordinates/VectorBytes;
   check cumulative allowance before consuming each coordinate.

5. Build the private borrowing retrieval context around `&GraphReadView` and
   its originating runtime/memory identity. It prepares a supplied vector for
   the admitted document declaration, preserving the borrowed input and exact
   mode intent; absent declared space, dimension mismatch and nonfinite input
   fail before storage work. It validates `Eligibility::Set` with `ids_for`
   against the actual view; AllIndexed and Set(empty) stay distinct. It retains
   no copied ID set, index, per-search allowance or Store reference. Charge any
   retained control object/reservation once to existing QueryMemory. Do not
   claim validation establishes query-tower/alignment compatibility beyond the
   existing supplied document-space contract or invent a new epoch protocol.

6. Extract the expected/actual version check from QueryMaterializer as a small
   allocation-free typed leaf. Keep physical row/error formatting at the legacy
   caller. Use it in native `resolve(expected DocumentVersion, runtime)` after
   the real GraphReadView lookup. Missing/tombstoned/stale live membership is an
   error, not None/a skipped hit. Return only a checked source-bound node handle
   with canonical optional text/vector access. Selected payload copying must
   use bounded reads and actual QueryArena/reservations; absence and present
   empty remain distinct. No `Store::get_documents_with_generation`, search
   admission, new source/catalog, or implicit whole vector/text materialization.
   TreeResources already counts native lookup/copy work: do not double count it.

7. Prove the adapter against retained actual native artifacts, then finish
   named regressions, feature compile checks and evidence. Stop once accepted
   checks pass. No optional refactor, generic framework or optimization pass.

## Named RED/GREEN checks

Use these test names (or report an exact justified renaming to root):

- `ze60_full_width_identity_and_staged_node_versions`: distinct IDs with equal
  low 64 bits, u128::MAX, checked zero inverse, differing revisions, actual
  normalized node delta conversion and relationship refusal. Use a tiny
  internal allocator-seeded fixture if both IDs must coexist; never fabricate
  millions of filler nodes. Product narrowing mutant must fail this test.
- `ze60_preparation_reuses_bounded_vector_validation`: legacy error parity and
  native present/absent space, dimensions, NaN/Inf, all four modes, cancellation,
  cumulative coordinate/byte limits. A supported native declaration above the
  legacy cap must not be rejected solely by that unrelated cap. No ranker.
- `ze60_rejects_foreign_eligibility_and_runtime_before_reads`: two real admissions
  with identical store/generation, plus same lease/memory but different runtime;
  genuine execution-owned EligibleNodeSet must fail before any lookup/map work.
  Matching view succeeds; omitted and explicit empty remain distinguishable.
  Product identity-check bypass mutant must make the test fail.
- `ze60_resolves_version_and_payload_from_retained_native_view`: real native
  nodes with graph-only, absent/empty text and original f32 bits; expected
  revision mismatch and absent/deleted expected member error. Hold an old real
  view across controlled bundle replacement, lazily resolve/copy through the
  adapter, and show old values stay old while a new view observes replacement.
  Reuse ZE-45's small actual old/new fixture, not the 5,405-edge split fixture.
  This proves the adapter under controlled replacement, not durable publication.
- `ze60_native_adapter_refusal_releases_charges_and_leases`: tightened actual
  QueryMemory and cumulative work limit, cancellation and deadline, close-first
  precedence, no partial successful payload, caller-owned copied value after
  scope, mapping/query/shared reservations and real lease registry return to
  baseline; actual close finishes after the callback exits.
- A scoped compile-negative/positive pair: returning the resolved native
  payload/reader beyond `NativeReadConsumer` scope fails for its lifetime;
  copying inside the scope into a lifetime-free owned value compiles. Adapt the
  established ZE-45 isolated compile probe pattern; do not weaken API lifetimes
  or export private types to make an external doctest possible.

Observe actual intended RED, smallest correction, terminal GREEN. Compilation
failure from a missing new symbol is useful progress but is not the substantive
RED for full-ID, foreign-view or stale-version behavior. If needed run a tiny
direct production mutant for those checks, retain its diff/log, restore exact
bytes, and rerun GREEN. No adversarial runner execution.

## Focused commands and completion

Run from the executor worktree with configured isolated nextest processes:

```sh
cargo nextest run --profile default -j 4 --retries 0 -p zeppelin-embed --features graph-cypher --lib -E 'test(ze60_)'
cargo nextest run --profile default -j 4 --retries 0 -p zeppelin-embed --features graph-cypher --lib -E 'test(exact_and_graph_tiers_preserve_quantizer_validation_errors) | test(native_read_scoped_consumer_retains_one_catalog_and_bundle) | test(native_read_cursor_rejects_same_view_memory_different_runtime) | test(native_read_graph_only_optional_payloads_and_full_width_ids)'
cargo check -p zeppelin-embed --lib --no-default-features
cargo check -p zeppelin-embed --lib --features graph-cypher
cargo check -p zeppelin-embed-workspace-tests --test adversarial_tests --no-default-features
cargo check -p zeppelin-embed-workspace-tests --test adversarial_tests --features graph-cypher
cargo check -p zeppelin-embed-workspace-tests --test adversarial_tests --features graph-result-test-support
git diff --check
```

Add only the existing focused legacy materialization regression(s) that exercise
the extracted equality leaf; identify their names before editing. Use changed-
path formatting and panic/indexing lint; record pre-existing unrelated lint
failures without fixing them. Do not run full suites or release builds.

Inspect runner source impact and record exact deferred changed-path obligations
for ZE-118. This owner authorization defers adversarial execution, coverage,
fuzz, size, soak, performance corpora and broad qualification; no evidence is
waived. Sparse index/checkpoint/recovery/GC, rank/anchors/ANN/fusion, search
operator/reports, compiler/ABI/Swift, platform/release qualification and any
publication coordinator edits remain excluded from ZE-60.

Evidence: brief README with source pin, hardware, exact commands, literal
RED/GREEN and mutant restoration, resource/counter assertions, lifetime proof,
feature controls and downstream limits. Preserve acceptance and inherited files.
Commit only owned changes with ZE-60 prefix and real RED/GREEN in the body;
return commit ID, paths/checks and limitations to root. No push or ticket close.

Escalate immediately after two fixes fail on the same issue, a missing producer
requires broader scope, or a required owner/lifetime seam contradicts this plan.
Send exact command/error plus the smallest next action. Report after 20 minutes
without an acceptance milestone. Do not tune fixture budgets, raise limits,
redesign storage, or substitute mock success to avoid escalation.
