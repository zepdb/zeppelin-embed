# ZE-55 main integration review

2026-09-19. Main before the repair is
`5c5b71d3299b543a4657dbd2fa6ab42f6503f421`, which contains ZE-55 cherry-pick
`b0e48ff` and evidence-only ZE-120 cherry-pick `5c5b71d`. The source candidate is
`5c9e757c05f977aa864a82fb3ab40630aff976b8`; pre-integration main is `8517c9e`.
All checks run from the real main checkout with separate target directory
`/tmp/ze-55-integration-target`, default nextest four-process isolation, no retries.
Hardware/toolchain are unchanged from the component evidence.

## Observed integration regression and exact restoration

The first source audit found unintended truncation in two resolved conflict
files. `tests/adversarial/runner.rs` ended immediately after the new PG11 probe,
losing the remaining function and 16,158 existing lines. `coverage.rs` ended
after the added coverage keys, losing the existing registry and remaining keys.
Three other conflict files lost one blank line. This was a conflict-resolution
script defect, not an intended manifest or product-code deletion.

Root restored complete pre-integration files and applied only the exact candidate
additions. The verifier `source-audit.py` independently reconstructs all six
shared files from full parent bytes plus the candidate delta. The named check
`integration_preserves_complete_parent_and_candidate_deltas` was replayed against
the immutable bad `b0e48ff` objects: **38/43 exact, exit 1**, identifying the two
truncations and three lost blank lines. Against repaired main it reports
**43/43 exact, exit 0**. This is source-preservation RED/GREEN, not a claim that
the broken integration passed compilation or tests.

The 37 nonconflicting candidate files are byte-for-byte exact. All six shared
files now equal complete parent plus complete candidate delta: core CLAUDE,
oracle module list, adversarial coverage/module/runner files and the adversarial
test file. The repaired runner is 761,270 bytes; coverage is 7,796 bytes. PG10 and
PG11 are both registered and executed. The source preservation check also retains
all preceding runner functionality. The repair changes five files because the
adversarial test file already contained the correct complete union.

## Focused integrated verification

| Check | Result |
| --- | --- |
| Entire frontend package | 46/46 nextest tests |
| Full public staging suite plus external compiler capacity | 29/29 |
| All staging internal allocation/preflight tests | 3/3 |
| PG10 and PG11 direct probes plus both actual runner tests | 4/4 |
| Independent staging and binding primitive oracles | 2/2 |
| Higher-ranked compiler lifetime compile-fail doctest | 1/1 |
| Original binding manifest and isolated source controls | Exact manifest, 4/4 controls |

That is **84 focused nextest tests**, separately from the doctest and Python
controls. Each actual runner seed-0 test executes 59 operations with zero
violations and proves every required key for its PG10/PG11 component. The full
28-test public staging suite and all 46 frontend tests remain present and pass.

Scoped all-target strict Clippy passes for core/oracle/workspace tests with
allocation-audit and test-support, and separately for the frontend package.
An initial combined lint invocation was invalid because enabling the core's
allocation-audit global allocator simultaneously with the frontend's independent
allocator test creates two global allocators. Splitting those legitimate target
configurations restores the intended audits; no source guard was changed.
Workspace formatting and `git diff --check` pass.

`commands.json` records exact argument arrays, target directory, exit status,
duration and raw-log hashes. Compressed logs retain successful checks and the
invalid combined-lint diagnostic. Source audit JSON and negative/positive logs
retain exact expected/actual hashes and lengths. `inherited-preserved.json`
verifies all 45 inherited files against their initial hashes. No inherited dirt
was staged; this worker's authorized commit contains only the five integration
repairs and this evidence directory.

No product implementation was changed during integration. Original TCK execution,
full workspace/adversarial campaigns, per-crate coverage, public language/store
qualification and platform/release gates remain ZE-118 and their owning tickets.
This review does not broaden the component claims in `ze-55-cypher-binding.md`.
