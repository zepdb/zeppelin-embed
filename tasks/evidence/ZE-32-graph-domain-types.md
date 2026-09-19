# ZE-32: native graph identity and value types

Status: accepted component implementation. Required tests, mutation controls,
per-crate line coverage and integrated checks pass after ZE-113 through ZE-116.
This is component evidence, not graph storage, Cypher, durability, platform or
first-release qualification.

## Authority and environment

- Ticket ZE-32, following owner acceptance in ZE-29; source/dependency boundary
  confirmed against ZE-112. The live tracker and `docs/graph/plans/identity.md`
  govern. ZE-109 supersedes the historical caller-drain DETACH paragraph.
- Source base: `366cdf4` (`git rev-parse HEAD`); existing dirty `.gitignore`,
  root `AGENTS.md`, `README.md`, `.agents/`, `CONTEXT.md`, `plan.md`, and
  `skills-lock.json` were retained.
- Host: Mac15,9, Apple M3 Max, 137438953472 bytes RAM; macOS 27.0, build
  26A5388g; arm64; rustc 1.93.0 (254b59607 2026-01-19), cargo-llvm-cov 0.9.0.
  Commands: `sysctl -n hw.model hw.memsize machdep.cpu.brand_string`,
  `sw_vers`, `uname -m`, `rustc -V`, `cargo llvm-cov --version`.
- Fixtures are constructor inputs and the existing deterministic overall
  adversarial workload, not the later graph performance corpus. No latency,
  persistence or oldest-supported-OS claim is made.

## Implemented boundary

`crates/zeppelin-embed/src/property_graph/` supplies nonzero, distinct u128
NodeId/RelId/StoreInstanceId; positive u64 GraphRevision with checked advance;
kind-scoped application keys; exact UTF-8 names; metadata separate from
properties; invariant-scoped batch-local references; homogeneous scalar-list
properties including typed and untyped empty lists; and finite-only supplied
vectors. Scalar/list F64 data preserves every IEEE bit pattern. Names and
property strings preserve NUL and Unicode bytes without normalization.

Constructors borrow caller storage and allocate nothing. They validate the
524,288-element list maximum, the 16,384-change local-slot bound, and individual
inputs against the 8 MiB input bound. Payload byte accessors deliberately exclude
canonical framing; ZE-33/37 must charge complete canonical and aggregate staging
bytes. Actual local-slot existence and vector-space/epoch admission remain the
staging/catalog owners' responsibility. No public DocId conversion, third-party
dependency, persisted format or legacy-ingest behavior changed.

## RED and GREEN

Tests use the approved public constructor/type seam. Each original test first
failed to compile against the absent API, then passed after its implementation:

| Test in `property_graph::tests` | Behavior |
|---|---|
| `full_width_identity_and_checked_revisions` | Zero rejection, all 128 bits, same-low/different-high IDs, revision overflow |
| `application_keys_and_metadata_preserve_kind_and_exact_utf8` | Entity-kind and namespace separation, composed/decomposed UTF-8, invalid UTF-8, metadata-kind rejection |
| `scalar_bits_and_typed_lists_are_distinct_from_finite_vectors` | NaN payloads, infinities, signed zero, five empty-list tags, count-zero sentinel, exact list/byte bounds, finite-vector distinction |
| `batch_local_references_are_bounded_and_distinct_from_durable_ids` | Existing/local distinction and slot-bound refusal |

Command: `cargo test -p zeppelin-embed --lib property_graph::tests`.
Raw original RED/GREEN logs are under `tasks/evidence/ze-32/` with
`identity-`, `keys-`, `values-`, and `local-` prefixes.

The compile failures were supplemented with actual product mutations. Truncating
all identity getters to u64 made the independent PG1 oracle fail (exit 101).
Then four deliberate product defects (identity truncation, bypassed metadata
kind guard, bypassed finite-vector guard, and an off-by-one local-slot guard)
made the four named tests fail: **0 passed, 4 failed**, exit 101. Each mutation
was restored byte-for-byte in a `finally` block. See `id-narrowing-red.log` and
`behavior-red.log`. These were temporary changes, not shipping fault switches.

Final review added `raw_name_admission_checks_byte_limit_before_utf8_work`:
RED returned InvalidUtf8 for oversized raw input where InputTooLarge was
required. The constructor now checks length before scanning UTF-8. Its RED is
`name-work-red.log`; terminal focused GREEN is **5 passed, 0 failed** in
`domain-terminal-green.log`. An additional
`all_scalar_and_nonempty_list_kinds_retain_their_values_without_vectors` case
checks every nonempty scalar-list kind, signed integer extrema, string bytes,
booleans and floating-list bits. Deliberately replacing returned data with an
untyped empty list made it fail (exit 101, `value-erasure-red.log`); restoration
was byte-for-byte. The final focused suite is **6 passed, 0 failed**.

Four compile-fail doctests prove NodeId/RelId and DocId separation, prohibit
escaping a local reference, and prohibit combining references from nested
batch scopes. Command: `cargo test -p zeppelin-embed --doc property_graph`;
**4 passed, 0 failed** (`doc-tests.log`).

## Independent adversarial evidence

The std-only independent oracle gains `property_graph::check`, accepting only
primitive inputs/observations. It compares full identities and revisions,
preserved scalar bits and vector admission; the latter derives finiteness from
the raw exponent bits independently of the production floating predicate.
`tests/adversarial/property_graph.rs` runs fixed zero/full-width/maximum cases
plus 64 seeded cases in every runner episode, using the existing seeded helper.
Four required coverage keys ensure the calls actually ran. This pure constructor
seam has no I/O, publication, cancellation or crash fault site to fabricate;
those campaigns belong with the implementation of those operations.

The focused harness and seven deliberately corrupted primitive observations
pass: `cargo test -p zeppelin-embed-workspace-tests --test adversarial_tests
property_graph`, **2 passed, 0 failed** (`probe-terminal-green.log`).

Before product changes, `cargo test -p zeppelin-embed-workspace-tests --test
adversarial_tests smoke -- --nocapture` passed **3 matching tests**, including
**224 overall episodes**, with zero violations (`adversarial-before.log`).
After implementation, the exact smoke command with `--exact --nocapture` passed
**1 test / 224 overall episodes**, zero violations (`adversarial-after.log`).
After the raw-name admission correction, the final exact smoke also passed:
**1 test / 224 overall episodes**, zero violations (`adversarial-final.log`).
Each of the zero/full-width/maximum domain coverage keys fired 224 times;
the seeded random-bits key fired 14,336 times (64 per episode).

## Gates and diagnostic history

- `cargo fmt --all -- --check`: pass.
- `cargo clippy -p zeppelin-embed -p zeppelin-embed-adversarial-oracle -p
  zeppelin-embed-workspace-tests --all-targets -- -D warnings`: final restored
  source passes (`clippy-restored.log`). Final formatting also passes
  (`fmt-restored.log`).
- `RUSTDOCFLAGS='-D warnings' cargo doc -p zeppelin-embed --no-deps`: pass.
- `cargo deny check`: advisories, bans, licenses and sources pass.
- Core-only diagnostic run: `cargo llvm-cov -p zeppelin-embed --lib --tests
  --fail-under-lines 90 --json --output-path
  /tmp/ze-32-qualification/core-coverage.json`: **1,242 passed, 0 failed,
  20 ignored**, 73 completed suites. Coverage was **60,971/82,241 lines =
  74.13698763390522%**, so the command exited **1**. This command omits the
  workspace adversarial consumers and does not satisfy the coverage gate.
  It also preceded the final raw-name correction and last value-kind test;
  it is retained as diagnostic evidence, not the final-source acceptance run.
- The complete first command from `scripts/coverage.sh` ran against final
  source: `cargo llvm-cov --workspace --fail-under-lines 90
  --ignore-filename-regex '(^|/)(registry/|crates/zeppelin-embed-bench|fuzz/|target/)'
  --json --output-path /tmp/ze-32-qualification/workspace-coverage.json`.
  Tests passed; coverage exited **1**. Final counts and prerequisite repairs
  are recorded below. No aggregate substitutes for the per-crate gate.

Raw logs and SHA-256 inventories are retained in `tasks/evidence/ze-32/`.
The initial failures below were retained while prerequisite repairs ran; the
terminal acceptance results appear in the final section.

## Isolated commit and integration

At the owner's request ZE-32 has a dedicated worktree/branch
`../zeppelin-embed-wt-ze-32` / `codex/ze-32-domain-types`, based on
`366cdf44ed5cd6b2cccb99e534eba09a58d9e274`. All 15 source inventory files
match the live coverage checkout byte-for-byte. The main checkout remains
unchanged while coverage runs. ZE-54 was started independently from committed
main without these changes. Individual accepted commits will be cherry-picked
into main, preserving the preexisting edits recorded above.

The independent read-only review found no concrete constructor-scope defects;
see `ze-32/independent-review.md` for its scope and limits. The separate Darwin
benchmark coverage lane from `scripts/coverage.sh` is running in the isolated
ZE-32 worktree with its own target/profile files:

```sh
cargo llvm-cov -p zeppelin-embed-bench --lib --test frontier \
  --fail-under-lines 90 \
  --ignore-filename-regex '(^|/)(registry/|crates/zeppelin-embed/|crates/zeppelin-embed-bench/src/(bin|platform|recall)/|fuzz/|target/)' \
  --json --output-path /tmp/ze-32-qualification/bench-coverage.json
```

Result: exit **1**, **135 passed / 0 failed / 2 ignored**,
**4,534/5,258 lines = 86.23050589577787%**. Logs and compressed report are
`ze-32/bench-coverage-red.log` and `ze-32/bench-coverage-red.json.gz`.
The benchmark sources and gate script match committed main exactly; ZE-113
owns baseline reproduction and repair in its own worktree. ZE-32 now depends
on that verified gate repair; no threshold/exclusion is relaxed.

### ZE-113 repair verified with ZE-32

ZE-113 closed with commit `2be6b2d0874225b9525610e6c9b16b8c0588d431`.
It was cherry-picked into the ZE-32 worktree as `024a720`, retaining a separate
prerequisite commit. The exact unchanged benchmark lane was rerun with ZE-32
present, using `bench-coverage-restored.json`/`.log`: exit **0**,
**148 passed / 0 failed / 2 ignored**, **4,744/5,258 lines =
90.2244199315329%**. The unchanged-source denominator remains 5,258.
The per-crate audit helper with `--source-root` naming the ZE-32 worktree and
`--require zeppelin-embed-bench` also exits 0. Raw logs, compressed report and
per-crate totals are retained under `ze-32/bench-coverage-restored*` and
`ze-32/bench-per-crate-restored.json`. The source/workspace run in main is
unchanged; its report excludes the separate benchmark lane by existing policy.

## Combined-worktree integration checks

Before any main integration, completed ZE-54 commit
`e503aefcd00756f2fe9aafa1236c302c6c14054e` was cherry-picked into the ZE-32
worktree as `e133a81`, after the separate ZE-113 prerequisite. The ZE-32
15-file source inventory still matches the live main coverage snapshot.
The following commands all exited 0 on this combined source:

```sh
cargo test -p zeppelin-embed --lib property_graph::tests
cargo test -p zeppelin-embed-cypher
cargo check --workspace
cargo deny check
cargo fmt --all -- --check
```

The focused domain suite has 6 passes and the parser suite 19 passes. Logs
are `ze-32/integration-{domain,parser,workspace-check,deny,fmt}.log`.
The final read-only follow-up review of ZE-54 at its committed source found
no defect in the additional unsupported-form/error-span changes; no accepted
syntax or complete-input check was lost. This is component integration proof,
not binding/execution/TCK/first-release graph acceptance. The main checkout
has not been modified by these isolated cherry-picks.

## Terminal workspace baseline and prerequisite repairs

The full workspace coverage command above completed with **2,075 passed,
0 failed, 50 ignored** across **128 top-level Cargo test binaries**. Counts
use the final result within each top-level Running block and exclude nested
child-process summaries. The full adversarial binary passed **424 tests,
0 failed, 11 ignored**, including the 48-seed x 11-family crash preset.
Its elapsed time was 2,342.69 seconds on the host recorded above.

Coverage was **68,246/76,584 = 89.11260837772902%**, exit **1**:

| Production crate | Covered / total lines | Percentage | Gate |
|---|---:|---:|---|
| Core | 62,339 / 69,167 | 90.12824034582965% | pass |
| FFI | 3,284 / 3,867 | 84.92371347297647% | fail, ZE-114 |
| Text | 2,623 / 3,550 | 73.88732394366197% | fail, ZE-115 |

FFI and text source bytes match committed main before ZE-32. Both repair
agents reproduced the exact deficits independently in clean worktrees.
The FFI baseline includes the workspace's existing abi-panic-probe feature.
The report and log are `ze-32/workspace-coverage.json.gz` and `.log`;
per-crate figures are `ze-32/workspace-per-crate.json`.

A supplemental read-only export of the same completed profiles disables
cargo-llvm-cov's default directory exclusions to expose the independent
production oracle library under tests/. The default report omits it entirely.
The supplemental command is:

```sh
cargo llvm-cov report --no-default-ignore-filename-regex \
  --ignore-filename-regex '(^|/)(registry/|crates/zeppelin-embed-bench|fuzz/|target/|rustc/)|/\.rustup/' \
  --json --output-path /tmp/ze-32-qualification/workspace-tooling-diagnostic.json
```

That export includes test-driver lines and its aggregate is **not acceptance**.
Only the oracle's src/ files are counted for its production denominator:
**13,986/16,250 = 86.06769230769231%**. ZE-32's new oracle contributes
25/25; the unchanged baseline is **13,961/16,225 = 86.0462249614792%**.
ZE-116 owns the external contract tests needed to restore that crate.
The workspace-tests package has only test targets and no production lib/bin;
it has no production-line denominator. Supplemental JSON and exact LCOV
exports are archived as `workspace-tooling-diagnostic.{json,lcov}.gz`.
No threshold, exclusion or claimed denominator has been relaxed.

All three repairs run in separate worktrees. Their tests will augment the
retained workspace coverage profiles only after source identity is checked,
and actual LLVM per-crate exports must pass before ZE-32 closes.

## Terminal acceptance after separate prerequisite repairs

ZE-113, ZE-114, ZE-115 and ZE-116 are closed with individually qualified
commits. They are integrated separately into main as be32cd0, 1359a58,
74be7ef and 8b705af respectively. The ZE-32 worktree retains each as a
separate cherry-pick, alongside the independent ZE-54 parser commit.
Only the 15 inventoried ZE-32 source paths and this ticket's evidence belong
to the ZE-32 commit. All 45 preexisting user files remain byte-identical.

The completed full workspace profiles were retained and backed up. After
verifying unchanged production bytes, only the newly added external test
targets augmented them. These commands each exited 0:

```sh
cargo llvm-cov --no-report -p zeppelin-embed-adversarial-oracle \
  --test storage_formats --test storage_observations --test metadata_contracts
cargo llvm-cov --no-report -p zeppelin-embed-ffi \
  --features abi-panic-probe,zeppelin-embed/test-support \
  --test ffi_boundary_validation
cargo llvm-cov --no-report -p zeppelin-embed-text \
  --features zeppelin-embed/test-support --test coreml_contracts \
  --test lifecycle_contracts --test runtime_contracts --test tokenizer_contracts
cargo llvm-cov report --fail-under-lines 90 \
  --ignore-filename-regex '(^|/)(registry/|crates/zeppelin-embed-bench|fuzz/|target/)' \
  --json --output-path /tmp/ze-32-qualification/workspace-coverage-green.json
```

In cargo-llvm-cov 0.9.0, --no-report implies --no-clean (cli.rs:1590); an
initial attempt spelling both flags was rejected before running anything.
The successful commands above preserved prior profiles. Added test targets
passed: oracle 13, FFI 15, text 12. No production mutation was present.
The report's aggregate is **69,048/76,584 = 90.15982450642431%**, exit **0**.
The explicit required per-crate audit also exits 0:

| Crate | Covered / total lines | Percentage | Evidence lane |
|---|---:|---:|---|
| Core | 62,342 / 69,167 | 90.13257767432447% | completed workspace + added tests |
| FFI | 3,484 / 3,867 | 90.09568140677528% | completed workspace + added tests |
| Text | 3,222 / 3,550 | 90.7605633802817% | completed workspace + added tests |
| Oracle | 14,698 / 16,250 | 90.44923076923077% | supplemental src-only subtotal |
| Benchmark | 4,744 / 5,258 | 90.2244199315329% | separate existing Darwin lane |

These are actual LLVM totals with unchanged denominators, not arithmetic
unions of line inventories. The isolated text ticket reports 3,245/3,550;
that separate run is not substituted for this integrated run's 3,222/3,550.
The final supplemental oracle export uses the earlier documented diagnostic
command with output `workspace-tooling-final.json`, counts only oracle/src,
and confirms the same 14,698/16,250 after all test additions. Its overall
aggregate still includes test drivers and is not used as a gate.

Final integrated strict core/oracle/workspace-test all-target Clippy and
workspace formatting both pass. The focused FFI/text package builds exposed
14 preexisting core dependency lints under a different feature configuration;
those failures are retained in the repair tickets, their crate-scoped lint
checks pass, and the original combined workspace configuration passes here.
No unrelated core lint edit was introduced. Independent source review,
full 2,075-test baseline, final 224-episode adversarial smoke, focused domain
tests/doctests and every restored failure control remain as recorded above.

This closes constructor/component acceptance only. Persistence, graph writes,
query binding/execution, full release size and platform qualification remain
with their dependency-ordered tickets. No missing production-model reference,
Windows/native Intel/minimum macOS, sanitizer or ignored soak claim is made.
