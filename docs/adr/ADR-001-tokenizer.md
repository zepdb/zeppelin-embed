# ADR-001 — Tokenizer build-vs-reuse

Status: PROPOSED, 2026-08-23. **Owner decision required.**
Task 12 Part A. Blocks nothing: Track L ships the owner-free branch
(option E, vendor-and-build) so the track keeps moving while this is open.

## The question

Task 12 needs word segmentation, Unicode folding, a Snowball English stemmer,
a stopword list, and a filter chain that emits `(term, position, byte offset)`.
Reuse a crate, vendor source into this tree, or write it here?

The spec framed this as a taste-and-size decision. It is not. It is a
dependency-budget decision, because **no candidate is on the allowlist**, and
the measurement below shows the smallest reuse option still breaks a standing
architecture invariant.

## What was measured

Method: an isolated staticlib crate per candidate outside the engine
workspace, so no candidate ever entered the engine `Cargo.lock`. Same release
profile as the engine (`opt-level="z"`, fat LTO, 1 CGU, `strip="symbols"`,
`panic="unwind"`). Post-strip linked-section total via the platform `size`
tool, `__LLVM` sections excluded — the identical method
`scripts/size-budget.sh` uses. Machine: M3 Max, Mac15,9. **Size only. No
timing was taken; this machine is not single-tenant right now.**

| candidate | linked | delta vs empty | transitive crates | blacklist audit |
| --- | ---: | ---: | ---: | --- |
| empty staticlib (control) | 315 KB | — | 0 | — |
| `unicode-segmentation` + `rust-stemmers` | 356 KB | **+41 KB** | 9 | **FAILS: pulls `serde`, `serde_derive`, `syn`** |
| `tantivy` 0.22.1 tokenizer stack | 1037 KB | **+722 KB** | 98 | **FAILS: `rayon`, `rayon-core`, `serde_json`, `zstd-sys`, `lz4_flex`** |
| `charabia` | not measured | — | — | not in the offline registry cache; not fetchable from this sandbox |
| turbopuffer tokenizer | not measured | — | — | no published artifact reachable from this sandbox |
| **vendor + build (option E)** | see task-12 delta below | — | **0** | **clean by construction** |

The two decisive findings:

1. **`rust-stemmers` 1.2.0 depends on `serde` unconditionally.**
   `default-features = false` does not remove it — the dependency tree is
   identical with and without. "Serde is absent from core production
   dependencies and JSON is confined to the benchmark tooling" is a standing
   root-`CLAUDE.md` invariant, so the spec's own default expectation
   ("own the pipeline, reuse the primitives") is not reachable as written.
2. **The tantivy stack pulls `rayon`.** Rayon is on the absolute blacklist,
   not merely off the allowlist. `zstd-sys` is a native C wrapper and
   `serde_json` is blacklisted for core. Three independent hard stops.

Size is no longer the discriminator. The static-library gate is 5 MB as of
2026-08-23 (root `CLAUDE.md`) and the engine is at 1610 KB, so even the
722 KB tantivy delta would fit. It is the dependency graph, not the bytes,
that rules these out. Under the owner's cardinal rule — best and fastest
lexical search — a candidate that cannot be admitted at all cannot be fast.

## Options

**A. `unicode-segmentation` + `rust-stemmers`.** +41 KB, tiny, well-pinned.
Requires the owner to admit `serde` + `serde_derive` + `syn` +
`proc-macro2` + `quote` + `unicode-ident` into core, reversing the
no-serde-in-core invariant and adding a proc-macro build step.

**B. tantivy tokenizer stack.** +722 KB, 98 crates, brings `rayon`.
Requires reversing the absolute blacklist. Also ships no English stopword
list (tantivy#2595, `research/03:422`), so the pipeline work is not avoided.

**C. `charabia`.** Unmeasured. Strongest CJK segmentation. Expected to be
the heaviest tree of the three. Needs network access to audit.

**D. turbopuffer tokenizer.** Unmeasured; no reachable artifact. The spec
itself says do not trust memory of it.

**E. Vendor and build (RECOMMENDED, and what ships now).** Hand-write the
segmenter, folding, stemmer, and filter chain in this tree. Vendor the
Snowball English stemmer algorithm (BSD-3-Clause) as Rust source with
attribution; vendor the Unicode data slices we use as generated tables with
their version pinned in the epoch digest. Zero new crates, zero
`Cargo.lock` change, byte-for-byte pinnable by definition — which is
decision criterion #1, the epoch requirement, and the only option that
satisfies it perfectly. An upstream release cannot silently change our token
stream because there is no upstream.

## Recommendation

**Option E.** It is the only option that needs no owner reversal, and it is
the only one that satisfies the epoch-pinning criterion absolutely. The
pipeline, versioning, and vocabulary layers are ours in every branch — those
are the product feature (`research/05` Tier 3 #10) — so options A–D only ever
saved us the segmenter and stemmer, and A and B cost an invariant each to do
it.

## What the owner's choice would change

- **If the owner picks A or B**, the segmenter and stemmer implementations in
  `src/fts/tokenizer/` are replaced and the epoch digest gains the crate name
  and exact version as digest input (already structured for this: the digest
  takes a segmenter id + version and a stemmer id + version). Conformance
  goldens change; that is an epoch bump, which is exactly the mechanism task
  12 exists to provide. Estimated churn: two modules, no format change.
- **If the owner picks C**, add CJK dictionary segmentation quality; the
  bigram fallback fixture in the conformance corpus is the thing that would
  improve. Same two-module churn plus an audit that is not yet possible from
  this sandbox.
- **Either way the epoch mechanism absorbs the change safely.** That is the
  reason to ship option E now rather than block: no decision is foreclosed.

## Deviations recorded here for the owner

- **NFKC scope (task-12 D3).** The plan chose a narrow versioned folding
  table over full NFKC because bytes were scarce. At a 5 MB gate that
  justification is void. See task-12 D3 in `src/fts/tokenizer/mod.rs`
  rustdoc for what actually shipped and why.
- **ADR location.** The spec says `tasks/adr/ADR-001-tokenizer.md`. `tasks/`
  is local-only by owner decision, so this lives in `docs/adr/` beside
  ADR-002..004.
- **Adversarial-runner extension.** `scripts/adversarial-smoke.sh` does not
  exist in this worktree base; task 11 has not landed. The spec's
  "add tokenizer-bearing docs to the closed vocab" extension is recorded as
  a backlog item, not built. No private runner was invented.

## Attribution for vendored material

- Snowball English (Porter2) stemmer algorithm: BSD-3-Clause, Martin Porter
  and Richard Boulton. Re-implemented in Rust from the published algorithm;
  attribution retained in `src/fts/tokenizer/stemmer.rs`.
- Unicode Character Database derived data: Unicode License (permissive);
  version pinned and folded into the tokenizer epoch digest.
