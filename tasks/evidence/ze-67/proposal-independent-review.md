# ZE-67 contract proposal: independent read-only review

Reviewed 2026-09-19 in `/Users/aghatage/Documents/code/zeppelin-embed-wt-ze-67`.
HEAD: `f9d81e0806ca68c876a26ed1983a3fffaf72b902`.
Proposal SHA-256: `9c2ce5af097c7c256121d24cc7433070e26bafaa324168187bf86a5e3844f6c5`.
Scope: proposal versus current bindings/identity/execution/Cypher/retrieval
contracts and actual core plan/value/catalog/runtime types. No builds, tests,
product changes, tracker writes or new claims. This is not implementation approval.

## Findings before contract freeze

1. **P1 — Explicitly represent the accepted search modes and hybrid output
   bindings instead of freezing only current ZE-48 variants.** Proposal lines
   53–59 promise coverage of current variants and preserve absent-vs-Auto, but
   current `query/plan/search.rs:94` has only `Exact` and `Approximate`. The
   accepted `cypher.md:84` specifies default/omitted, Auto, Exact and Scan and
   explicitly does not adopt an approximate mode. Further,
   `query/plan/mod.rs:293–303` provides only node and score output slots, while
   `cypher.md:82` and retrieval.md:74 require hybrid node/score plus nullable
   vector_distance and lexical_score. Per-call reports do not replace those
   per-row component bindings. Before pinning C tags/layout, name the full tier
   table and independently optional component output slots (or an equally
   explicit lossless representation), with an exhaustive mapping table. Record
   that current core adaptation remains a later implementation obligation; do
   not map Scan/Auto/default to Approximate or drop YIELD components. The
   proposal already mentions absent-vs-Auto; that sentence alone does not
   resolve the current enum and output-slot mismatch.

2. **P2 — Specify document-only compatibility when reusing ZeEmbeddingEpoch.**
   Proposal lines 60–64 reuse the existing full epoch pointer. That legacy
   descriptor (`ffi/src/abi.rs:1090`) includes document, query and alignment.
   Current `catalog/interpretation.rs:7,106–120` deliberately stores only the
   document tower, and retrieval.md:53 requires a compatible query-tower swap
   not to change graph/document identity. The proposal does not say whether
   open compares the full pair, ignores the query fields, or retains them as
   separate runtime query interpretation. Define that split before freezing the
   open declaration: extract full document fields for stored compatibility,
   describe query/alignment validation/use separately, and preserve absent
   embedding independently of tokenizer identity. Full-epoch reuse can work;
   treating its aggregate digest as graph identity cannot. Also explicitly mark
   reused legacy ZeEmbeddingEpoch/ZeEmbeddingTower as frozen unversioned legacy
   descriptors, rather than implying they have the new abi_size prefix.

## Nonblocking precision notes

- The proof phrase “cross-kind rejection” (line 98) should mean tagged
  entity/value/reference domain mismatch or compile-time C type separation.
  High/low pairs alone cannot prove whether a caller reconstructed node bits
  from a relationship or another store; bindings.md:51 expressly limits ID
  origin inference. Do not promise rejection based solely on opaque bit origin.
- Keep public operation/work discriminants explicit and independently pinned;
  the current Rust WorkKind has repr(usize) with implicit ordinal values. A
  mapping to its 22 categories is appropriate, a public cast freeze is not.

## Boundary checks with no finding

The separate graph-only header, default-disabled feature, no unavailable runtime
function declarations and unchanged symbol allowlist correctly avoid adding
graph claims to legacy packages. The proposed macOS/aarch64 selection refusal
is consistent with ZE-103, provided ZE-107 still owns deployment target >=14.0,
arm64-only graph packaging and actual minimum-runtime qualification. No current
macOS 14 runtime proof is implied. Keeping old errors/layouts and appending
reviewed error distinctions is consistent with bindings.md. Exact old goldens
and generated output remain necessary focused gates.

Optional payload presence, typed empty-list preservation, complete result
disposition/diagnostic fields, precommit registry ownership boundary and no
production TCK counter addition match the accepted contracts. Root/owner can
resolve the two representational findings without broad tests or runtime stubs.

Owner response during review: ZE-67 owner agreed to explicit tier presence and
Auto/Exact/Scan fields, optional hybrid component output slots, no Approximate
fallback/component drop, and document-only stored interpretation with separate
query/alignment metadata. ZE-69/64/58 retain adaptation/refusal ownership. This
records the response; a revised proposal or implementation was not reviewed.
