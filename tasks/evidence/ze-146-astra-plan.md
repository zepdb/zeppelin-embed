# ZE-146 — bounded native lexical producer

Status: root-approved for GPT-5.6-Sol/xhigh execution. Inspected source pin: `70f8cc0a645bf85577d8d034622c291e586acaa9`. Execution may pin current main `7fd76c761b099e7d1efaf5e3e53fde709764708f`: its intervening ZE-145 expression integration is disjoint and the relevant FTS source is unchanged. ZE-146 is claimed and dependency-ready. This plan is source inspection only; no product edit, build or test has run. Follow `/tmp/graph-sol-executor-rules.md` throughout.

## Result and retained boundaries

Implement one complete graph-gated producer over the existing analyzer and FTS representation. It accepts borrowed documents, excludes analyzed-empty documents, assigns dense lexical rows, and returns accounted encoded/decoded lexical data. Its decoder also supports an authentic query owner. ZE-61 still owns full NodeId/revision mappings, sparse masks, interpretation/root binding, artifact publication, admission, compaction and reopen integration. No whole-corpus text/token collection, new tokenizer, new dependency, independent allowance, public graph operation or replacement budget is authorized. A fragment contains only the documents the caller incrementally pushes for one staged batch; the existing 32 MiB storage-preparation ceiling applies.

Source contracts: `docs/graph/plans/retrieval.md` (G/V/T and native lexical-preparation addendum), `qualification.md` (shared/nested capacity, actual work and deferred ZE-118 campaign), `parallel-contracts.md` (ZE-146 prerequisite of ZE-61). Preserve every original ZE-61 gate.

The concrete gaps are `fts/tokenizer/mod.rs:398` and `pipeline.rs:485-521` (whole uncharged vectors, stable rank order and HashSet dedup), `fts/index.rs:212,401,482` (opaque HashMap allocation and position-based lengths), `fts/sealed.rs:241,363,441,1121` (controlled sealing still allocates freely; region codec and validation allocate without control), and `fts/postings.rs:562,833,1025,1563` (existing encoding/validation rules and partially controlled leaves). Reuse those algorithms. Do not substitute an estimate such as `HashMap::capacity() * size_of::<entry>()` for exact allocation ownership.

## Frozen consumer API

Expose `pub mod graph_build` from `fts/mod.rs` only under `graph-cypher`. Types below live there; imports are the existing `fts::tokenizer::{Analyzer, TokenizerEpoch}`, `fts::sealed::{SealedSegment, SealedSegmentError}`, `fts::postings::PostingsError`, `property_graph::storage::memory::StorageMemory`, `property_graph::storage::tree::directory::{TreeResources, TreeError}`, `property_graph::query::resources::QueryMemory`, and `property_graph::query::runtime::RuntimeContext`.

```rust
pub struct GraphLexicalBuilder<'a, 'm> { /* private */ }
impl<'a, 'm> GraphLexicalBuilder<'a, 'm> {
    pub fn new(analyzer: &'a Analyzer, memory: &'m StorageMemory<'m>,
               resources: &mut TreeResources<'_>) -> Result<Self, GraphLexicalError>;
    pub fn push_text(&mut self, text: &str, resources: &mut TreeResources<'_>)
        -> Result<Option<u32>, GraphLexicalError>;
    pub fn finish(self, resources: &mut TreeResources<'_>)
        -> Result<PreparedGraphLexical<'m>, GraphLexicalError>;
}
pub struct PreparedGraphLexical<'m> { /* private, not Clone */ }
impl<'m> PreparedGraphLexical<'m> {
    pub fn region(&self) -> &[u8];
    pub fn decoded(&self) -> &DecodedGraphLexical<'m>;
    pub fn owned_bytes(&self) -> usize;
}
pub struct DecodedGraphLexical<'m> { /* private, not Clone */ }
impl<'m> DecodedGraphLexical<'m> {
    pub fn decode_prepare(bytes: &[u8], epoch: TokenizerEpoch,
        memory: &'m StorageMemory<'m>, resources: &mut TreeResources<'_>)
        -> Result<Self, GraphLexicalError>;
    pub fn decode_query(bytes: &[u8], epoch: TokenizerEpoch,
        memory: &'m QueryMemory<'m>, context: &mut RuntimeContext<'_, '_, '_>)
        -> Result<Self, GraphLexicalError>;
    pub fn epoch(&self) -> TokenizerEpoch;
    pub fn row_count(&self) -> u32;
    pub fn row_lengths(&self) -> &[u32];
    pub fn total_tokens(&self) -> u64;
    pub fn sealed(&self) -> &SealedSegment;
    pub fn owned_bytes(&self) -> usize;
}
#[derive(Debug)]
pub enum GraphLexicalError {
    Tokenizer(crate::fts::tokenizer::TokenizerError),
    Postings(PostingsError),
    Region(SealedSegmentError),
    Resource(TreeError),
    Failed,
}
```

Implement Display/Error and From for the four wrapped errors. Preserve typed cancellation/deadline/runtime/memory errors; query allocation errors map through `TreeError::Runtime(RuntimeError::Memory(...))`, not a generic geometry error. Checked length/offset/count overflow is `Region(Geometry(<specific static detail>))`; document input beyond the analyzer's existing maximum is `Tokenizer(TextTooLong { ... })`.

`new`, each `push_text`, `finish` and `decode_prepare` call the existing `TreeResources::require_preparation(memory)` before work/allocation. Query decode checks `std::ptr::eq(context.memory(), memory)` and `context.checkpoint()` before work/allocation; mismatch is `Resource(TreeError::Invalid("lexical query owner mismatch"))`. Existing memory lifetimes can be shortened to `'m`, as already done by `TreeResources::for_query`; no owner adapter or new runtime constructor is needed.

The builder borrows the supplied analyzer, does not recompile/clone/default it, and caches its actual epoch. Successful empty, whitespace-only, and stopword-only analysis returns `None` without advancing row count; nonempty analysis returns `Some(previous_row_count)` and increments with checked arithmetic. Row length is **max surviving token position + 1**, not token count or number of unique terms; stopword gaps and stacked variants retain current meaning. Any failed push marks the builder terminal; later push/finish returns `Failed`. A failed consuming finish/decode returns no partial prepared object and drops every local owner. This permits private partial mutation without exposing a successful prefix.

The graph representation is single `DEFAULT_FIELD`. Graph decode accepts the producer's shape: zero rows/zero fields for an empty fragment, or exactly DEFAULT_FIELD with positive per-row lengths for nonempty rows. It retains the ordinary codec's validations and rejects other shapes explicitly. Cached total_tokens is a checked sum computed during controlled build/decode, so the accessor never performs an uncontrolled long fold. `row_lengths` borrows the sealed default-field length array. No method extracts a bare Vec or SealedSegment; the caller appends borrowed `region()` bytes to its accounted artifact owner, reads/copies required row statistics, then drops the prepared owner. No optional `into_region` API is needed.

**Epoch qualification:** ZFTS region version 1 has no epoch field. Decode's epoch argument comes from the caller's already-validated enclosing descriptor and bound interpretation; it does not authenticate these bytes by itself. ZE-61 must compare descriptor epoch with its retained interpretation before calling decode. Do not change the region to add an epoch, and do not call a supplied epoch a verified property of bare bytes.

## Strict file ownership

All paths below are beneath `crates/zeppelin-embed/` unless stated otherwise:

- New `src/fts/graph_build.rs`, `src/fts/graph_build/memory.rs`, `src/fts/graph_build/tests.rs`.
- `src/fts/mod.rs`: graph-gated module registration only.
- `src/fts/control.rs`: private capacity-aware build containers/policies and fallible bounded compare/sort/copy support, preserving existing WorkCheck callers.
- `src/fts/tokenizer/{mod,pipeline,segment,fold,stemmer,numbers,vocab}.rs`: only shared controlled analysis leaves/wrappers and their focused tests. No generated fold table, stopword table, configuration/epoch algorithm or fixture edits.
- `src/fts/{index,postings,sealed}.rs`: shared grouping/input-view/encoder/decoder leaves necessary below; unchanged public legacy signatures and bytes. Do not rewrite SegmentIndex's legacy HashMap.
- `src/property_graph/storage/memory.rs`: only widen `reserve`, `StorageReservation` and its `bytes`/`resize` to `pub(crate)`; no policy or algorithm changes.
- New `tests/graph_lexical_prepare.rs`; `Cargo.toml`: only its `[[test]]` entry requiring `graph-cypher`.
- New root evidence `tasks/evidence/ze-146-bounded-lexical.md`.

Do not edit GraphResources, lifecycle accounting, query/runtime/resources, catalog/view/retrieval/native storage, canonical plans, dependencies, other tests or fixtures. ZE-147 owns resource-interval files; ZE-145 owns expressions. Root reconciles additive Cargo registrations. An indispensable additional path requires root approval before editing.

## Ordered implementation

1. **Establish the small RED.** Add `lexical_rows_skip_analyzed_empty_and_match_region_golden`. Before the new producer exists, a test-local baseline calls real `SegmentIndex::push_document` and wraps its dense returned row in `Some`; the assertion that `"the and"` produces `None` must fail with `Some(0)`. This records the actual missing graph behavior without changing the legacy contract. Replace that test-local baseline with GraphLexicalBuilder as soon as the producer compiles. An unresolved import/compiler failure alone is not semantic RED.

2. **Add one private allocation/control policy and guarded containers.** Keep the existing `WorkCheck` public-to-crate signature usable by every current caller. Add generic private build helpers in control.rs: a policy with associated error and charge types; guarded Vec/String owners whose backing drops before its guard; explicit fallible allocation/growth, chunked copy, work/checkpoint and byte-comparison operations. The legacy policy has a zero-sized charge and infallible allocation/checkpoint behavior; the graph policy maps to the existing storage/query guards. Use an exhaustive `Infallible` match in legacy analyzer wrappers, not unwrap/panic/fallback. This is a lexical-build helper, not a new execution/memory subsystem.

   Graph memory.rs defines the private owner enum `Preparation(&'m StorageMemory<'m>) | Query(&'m QueryMemory<'m>)` and charge enum over **actual** `StorageReservation<'m>` / `QueryReservation<'m,'m>`. Charge each top-level owner descriptor and every actual heap capacity (including nested strings/positions and descriptor vectors). Reserve requested checked bytes before `try_reserve_exact`, reconcile actual capacity immediately, and drop backing on reconciliation failure. Follow the existing allocation-audit attributed-allocation convention. Growth allocates a separately reserved replacement while the old allocation remains charged; move/copy initialized elements in bounded steps, then drop old backing/charge. No `.reserve`, `.collect`, `.clone`, formatting String, HashMap/HashSet/BTreeSet, stable-sort scratch, or hidden Vec growth may bypass this policy on the graph path. Capacity ownership moves with buffers; never momentarily release a live backing's charge or keep a freed scratch allocation charged as purported exact retained capacity.

3. **Thread the policy through the existing tokenizer algorithm.** The common analysis implementation is one algorithm; legacy Analyzer wrappers and graph building call it with different policies. Keep stage order, rank, flags, original offsets, vocabulary preference, number rules, stemming and saturation behavior identical. Internal emission/token terms retain their string guards until dropped or moved. Existing public `Token`/`Analyzer` signatures do not change.

   Control every input-sized traversal/allocation: segmentation and CJK boundaries, part ranges, surface flag scans, folding (including expanding folds), term cloning/catenation, surface-position tables, vocabulary runs/canonical copies, number runs/joined strings, stopword/stem filtering, stemmer char buffers/scans/output, final ordering and dedup. Sharing emission visitors with existing segment/fold/number rules is allowed; a second copied tokenizer is not. Bounded static suffix/number tables need no new dynamic machinery. The stemmer's step-5 body can borrow its existing prefix rather than allocate a duplicate. Reject long digit strings by the already-existing 12-byte bound before scanning digits; the answer remains identical. Vocabulary controlled lookup may scan existing borrowed entries with controlled exact comparisons, preserving longest-run-first selection; do not clone the analyzer map or add a cache.

   Graph ordering uses allocation-free fallible heapsort (extend/reuse control.rs's implementation) with a checked emission ordinal as the final tie-breaker to reproduce the existing stable sort. Comparators must poll within long equal prefixes. Dedup uses accounted indices sorted by `(position, term, original sorted index)` and a keep marker; retain the earliest original sorted index per `(position,term)`, then emit retained entries in the original sorted order. This preserves the HashSet's first-survivor behavior even when equal terms occur at different ranks. The legacy public stream remains byte-for-byte identical; no epoch bump or golden update is authorized.

4. **Build dense lexical postings without opaque hash allocations.** In graph_build.rs, use guarded Vec-backed term entries and posting/position buffers. Group each document's borrowed analyzed terms by `(term bytes, position)` using the same grouping rule extracted from `index.rs:482`; unique positions determine tf, and document length uses the max-position rule above. Locate existing graph term entries by a controlled exact comparison; maintain explicit deterministic ordering before sealing. Do not retain input text or all documents' analyzed tokens after each push. Guarded aggregate postings are the index under construction, not an additional corpus copy.

   Add a private borrowed posting/list view for the existing encoder to consume both legacy PostingList and guarded graph postings. Its content is `(docid, tf, &[u32])`; it must not clone or materialize an uncharged PostingList. Factor shared sealing from ordered `(term, field, posting-list-view)` plus row lengths, preserving existing block geometry, terms order, union summaries, impacts and every emitted byte. Graph uses only DEFAULT_FIELD, so each term's union df equals its df and field_count is 1. Empty builder sealing remains valid.

5. **Make the shared codec leaves allocation-aware and fully controlled.** Reuse `block_impacts_controlled`, `encode_v2_controlled`, `PostingsReader::open_controlled` and sealing logic through private policy-taking cores; leave legacy wrappers intact. This includes metadata Vecs, docid/tf/position streams, per-block scratch, impacts, terms/spans/blob, row arrays, final encoded bytes and reader block metadata. Before allocating from decoded header counts, check existing geometry against available bytes with checked arithmetic; malformed counts cannot force a huge allocation first.

   Refactor the existing whole-region encode/decode field and byte loops into shared controlled leaves. A graph decoded owner contains the ordinary private SealedSegment plus the guards for its exact backing capacities, in backing-before-guards drop order. Since graph has at most one field, final sealed backing guard slots can be fixed-size (terms, spans, blob, outer lengths, sole inner lengths, totals); do not build an unaccounted guard registry. Temporary buffers retain their own guards until ownership transfer is complete.

   Do **not** enter `validate_decoded_spans`' existing whole-segment `decoded_lists`/BTreeSet materialization on the graph path. Share its header/span/metadata checks, then validate one posting block and its position stream at a time using accounted metadata and bounded scratch. Reuse the actual postings decoder's bit/ordering rules; reject zero tf, nonascending docids/positions, invalid/truncated geometry and out-of-range row ids. For the single graph field, verify field_count=1 and union_df=df; no multi-field union collection is needed. Leave existing multi-field legacy validation behavior intact. Retained decoded data and the encoded finish buffer remain charged simultaneously until dropped.

6. **Complete real control and ownership, then terminal GREEN.** Storage operations use the same passed TreeResources for their entire call sequence; never reset a local work budget per document/stage. Call `step(actual_units)` before each bounded unit of preparation work, including unsuccessful probes/comparisons, and check at entry/exit/before allocation. A character/metadata/posting visit is a unit; byte copy/comparison charges its actual byte count. No callback-count surrogate or blanket estimated work charge. Poll at most every 64 element/character operations and every 8,192 copied/compared bytes, including one long token, sort comparators and position streams. The existing WorkCheck can supply legacy cancellation cadence; its zero-argument callback by itself is not an exact work counter.

   Query decode calls the actual RuntimeContext directly: checkpoint inside every long loop and before allocation; charge `LexicalBlocks` for every block visited and `LexicalPostings` for every posting visited across every validation pass; charge `CopiedBytes` for actual copies. Avoid duplicate metadata/decode passes where unnecessary, but count repeated visits if present. Use `check_work` before a fallible copy and charge only the copied chunk. Preserve close-first lifecycle rejection. Do not add WorkKind variants or count metadata loops as operator rows. The query runtime remains usable after decoder return; only its QueryMemory guard is retained by the result.

## Exact small checks

Implement these named tests, using existing GraphResources/WriteMemory/StorageMemory and QueryMemory/RuntimeContext constructors and small owned fixtures. Internal tests may use private scoped hooks; do not add a production fault framework.

- `lexical_rows_skip_analyzed_empty_and_match_region_golden`: `""`, whitespace and `"the and"` return None; `"bronze zeppelin"` and `"silver zeppelin"` yield rows 0 and 1, lengths `[2,2]`, total 4, actual analyzer epoch, and **exact existing** `fixtures/format/postings_segment_region_v1.hex`. Decode with each owner and compare the same lengths/row count. The first RED is the literal Some(0)-versus-None baseline above.
- `lexical_positions_keep_gaps_and_stacked_variants`: default `"the bronze and zeppelin"` yields length 4, stemmed `bronz` at position 1 and `zeppelin` at 3; `"twenty five"` retains the existing number variants without making length exceed 2. Inspect literal postings/positions, not only equality to another production encoder. In the tokenizer's controlled unit tests, compare all ten existing conformance input streams to their committed token goldens, plus a configured vocabulary case and the existing rank-collision case (`B#` with the combining mark from pipeline's dedup comment).
- `lexical_capacity_is_owned_through_finish_decode_and_drop`: independently sum private actual Vec/String capacities and descriptor charges for the small fixture; compare to the owning memory's current delta while builder, prepared, and decoded owners live. Drop must restore the exact pre-call baseline. Tight preparation and query allowances reject before a required growth; keep old+replacement live peak in the assertion. Use a scoped deliberate omitted-charge mutation once to observe the intended assertion fail, restore it, then require GREEN. Do not infer exact allocation ownership only from the producer's owned_bytes accessor.
- `lexical_owner_mismatch_and_failed_builder_are_terminal`: wrong preparation owner and wrong query memory fail before allocation; a failed push never permits finish of the earlier successful prefix. Verify memory baseline after consuming failure/drop.
- `lexical_long_work_stops_inside_analysis_sort_and_codec`: deterministic scoped checkpoints request real cancellation during a long single token, repeated-term sorting, and position/region encode/decode. Use at most 128 KiB inputs, no timing sleeps. Show a positive work counter/probe before failure and no successful partial owner. Tight TreeResources work limit must fail during work; already-expired deadline must preserve its typed error. Include successful controls using the same small inputs.
- `lexical_query_decode_charges_actual_work`: a small real region under authentic RuntimeContext has literal expected lexical block/posting counts; limits of zero for either kind reject the corresponding nonempty decode. Verify copying counters against the actual copied buffers and show close-before-cancel precedence using the existing retained-view test pattern. This proves the producer's compiled runtime boundary, not native GraphStore admission.
- `lexical_decode_rejects_corruption_without_retaining_memory`: use the existing two-row region, change reserved header byte 20 to 1 and truncate the final byte. Additionally encode one row `"zeppelin zeppelin"` (positions `[0,1]`) and clear its second position delta to create `[0,0]`. Use literal fixture offsets derived once from the frozen layout, record them in the test, and check the expected typed error plus restored memory. Add a non-default/multiple-field shape rejection using an ordinarily encoded legacy fixture. Never alter golden files.

Run only these focused commands (repeat just affected named tests while fixing):

```sh
cargo nextest run -p zeppelin-embed --features graph-cypher --test graph_lexical_prepare -j 4 --retries 0
cargo nextest run -p zeppelin-embed --features graph-cypher --lib -E 'test(fts::graph_build::tests::) | (test(fts::tokenizer::) & !test(fts::tokenizer::properties::)) | test(fts::index::tests::rows_are_dense_and_ascending) | test(fts::index::tests::document_length_counts_positions_not_tokens) | test(fts::index::tests::posting_lists_carry_positions) | test(fts::sealed::tests::astra_18_) | test(fts::control::tests::astra_18_)' -j 4 --retries 0
cargo nextest run -p zeppelin-embed --test tokenizer_conformance --test fts_format_golden --test postings_region_golden -j 4 --retries 0
cargo check -p zeppelin-embed --lib --no-default-features
cargo check -p zeppelin-embed --lib --no-default-features --features graph-cypher
cargo check -p zeppelin-embed --lib --no-default-features --features graph-cypher,allocation-audit
git diff --check
```

The scoped omitted-charge and cancellation controls belong to unit tests in the owned files. The `fts::tokenizer::properties` campaign (nine properties with 512 cases each) is explicitly deferred to ZE-118; all named component tests, deterministic tokenizer leaves, conformance/goldens and controlled leaves above remain required. No full FTS/search suite, workspace command, adversarial execution, performance, coverage, fuzz or release build. Do not expand the default analyzer regression into an audit campaign. The source change stays sequential and introduces no publication/concurrency behavior; no new runner operation is introduced. Broad runner/allocation qualification remains ZE-118.

## Handoff and escalation

Report the first intended RED, core compile milestone, and focused GREEN to root. Follow the rules file: two unsuccessful fixes of the same failure or 20 minutes without an acceptance milestone triggers a precise root update. Stop before altering a public signature above, an epoch/format/golden, the memory ceiling, a borrowed owner lifetime contract, or an unowned file. In particular, do not resolve a generic-policy/lifetime difficulty by calling an uncontrolled legacy analyzer/codec inside the graph path or by returning an unguarded buffer.

Record exact RED/GREEN commands, actual source pin, changed files, memory/work/corruption cases and unrun broad gates in the one evidence file. Commit only the allowlist with a `ZE-146:` subject and literal RED/GREEN body; return commit/hash/checks to root. Leave tracker in_progress for root integration/close. ZE-61's consumer must target these signatures only after ZE-146 compiles and integrates; this plan is not a substitute producer.
