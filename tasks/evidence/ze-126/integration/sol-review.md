# ZE-126 main integration preservation review

Verdict: **PASS — no preservation or seam-integration blocker found.**

## Scope

Read-only review of candidate `d2e36c7655ee95ccdbc8fa891da93b2294d9531b`
(parent `beb1a1e44730b56494ec2cb4aafc5d9ea5419763`) integrated without a
commit onto main `0e0064ddb3b1630e8c18050cb78c30ac7bcfb638`. This review checks only
candidate-byte preservation, the five shared adversarial registration seams,
the post-integration strict-lint correction, and compatibility with main's
existing QueryArena detach/completed-result storage. It does not repeat the
candidate semantic review or broad suites deferred to ZE-118.

The repository and tracker were not modified by this reviewer. The only file
written is this report under `/tmp`.

## Candidate correspondence

The refreshed `/tmp/ze-126-main-integration/correspondence.json` covers all
127 candidate paths:

- 121 paths are byte-exact to the candidate.
- Five shared registration files are additive combinations.
- `tests/adversarial/graph_lowering.rs` has one reviewed strict-lint-only
  delta. The pre-lint correspondence remains in
  `correspondence-before-lint.json`; the exact delta is retained in
  `graph_lowering-lint-correction.diff`.

Within the candidate's 36-path owned source allowlist, 30 paths independently
matched the candidate in both the staged index and worktree. The remaining six
are the five additive registration seams and the one lint correction described
below. There are no Cargo manifest, lockfile, or deny-policy changes.

## Five additive registration seams

For each shared file, the main-to-live diff contains zero removals. Its added
lines match the candidate parent-to-candidate added lines byte for byte:

| Path | Added | Removed | Candidate additions exact |
| --- | ---: | ---: | --- |
| `tests/adversarial-oracle/src/lib.rs` | 2 | 0 | yes |
| `tests/adversarial/coverage.rs` | 12 | 0 | yes |
| `tests/adversarial/mod.rs` | 2 | 0 | yes |
| `tests/adversarial/runner.rs` | 1 | 0 | yes |
| `tests/adversarial_tests.rs` | 37 | 0 | yes |

This is stronger than a conflict-marker check: every pre-existing main line in
these files remains in order, and the only live additions are the candidate's
PG17 lines.

The prior registrations and focused tests remain present once each at their
expected seams:

- PG8 `graph_directories`: module, runner probe, and two focused tests.
- PG14 `graph_relational`: oracle/harness modules, runner probe, and two focused
  tests.
- PG15 `graph_completed`: module, runner probe, and two focused tests.
- PG16 `graph_response`: oracle/harness modules, runner probe, positive tests,
  and the feature-off absence test.

PG16's `graph-cypher` gates remain intact on its harness module, runner probe,
coverage keys, and positive tests; the `not(feature = "graph-cypher")` absence
test also remains intact.

PG17 is registered exactly once at each required seam: one oracle module, one
harness module, one runner probe, one direct probe test, and one runner-episode
test. All 12 `property-graph.lowering.*` smoke keys occur exactly once in the
coverage registry.

## Strict-lint correction

The original candidate used `for case in 0..5` and indexed
`REQUIRED_COVERAGE`, which strict Clippy rejected as `needless_range_loop`.
The retained RED is
`/tmp/ze-126-main-integration/runner-clippy.log`.

The only candidate-to-live source delta replaces that loop with enumeration of
an explicit five-key array. The array maps cases 0 through 3 to the same
`REQUIRED_COVERAGE[0..=3]` entries and case 4 to the same
`property-graph.lowering.completed-path` literal. The `case` sequence remains
0 through 4; all trial, oracle, mutation, fault, counter, and release logic is
unchanged. Candidate SHA-256 is
`e3778009b1c10a4e45bf74cbe31f43251613f772ddd3b234c6d1a4a3119f4034`;
integrated index/worktree SHA-256 is
`353028fbc9acc60204d6b896b4c7606052800ffbccc0d48481c40337cc0d5c9d`.

## QueryArena and completed-result seam

Main's completed-result implementation is byte-exact in the live tree for:

- `query/resources.rs`
- `query/completed.rs`
- `query/completed/records.rs`
- `query/completed/validate.rs`
- `query/mod.rs`

The APIs coexist without an ownership ambiguity:

- Main's `QueryArena::detach_owned(self)` consumes an arena and is sealed to
  `completed::OwnedElement`. `PreparedGraphResult::detach` uses that path for
  its 12 completed typed pools.
- ZE-126 adds `validate_plan` only for `QueryArena<NodeFacts>`. It borrows the
  arena mutably and returns a `GraphPlan` plus `RetainedAllocation` tied to that
  fact-arena lifetime and existing `QueryMemory` identity. `NodeFacts` is not a
  completed `OwnedElement`, so this seam cannot enter the detach path.
- ZE-126's `execute_in` delegates to the existing `drain` with the caller's
  existing `RuntimeContext`. It does not create or reset a context, and the
  existing completion/final admission checks remain the shared implementation.

Therefore the borrowed plan/fact-owner path and main's consuming completed
storage path preserve their separate authority and release rules.

## Validation boundary

Root reported the frozen integrated source passed 29 frontend tests, 65 core
tests, and seven combined graph-Cypher runner tests, including the retained
PG8/PG14/PG15/PG16 tests and PG17; the PG17 runner completed 59 operations with
zero violations. After the lint-only correction, the strict combined-runner
lint, both affected PG17 tests, and source formatting also passed. Root retains
the exact commands, outputs, exit codes, and elapsed times in the adjacent
integration records and logs. This reviewer did not duplicate those runs.

Broad workspace/adversarial campaigns and coverage qualification remain
deferred to ZE-118; this review makes no claim for them.
