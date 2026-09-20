# ZE-45 Standards re-review

Scope: exact delta `3a0ac9dc6e9419a3709e5304ade721915cbf5991..ebb5f411ce908fa4c6e100a7f89e9d82c0b5f250`, source review plus existing narrow logs only. No tests or adversarial runner were executed.

## Hard documented-standard findings

- **Medium — exact query allocation accounting is false** — `crates/zeppelin-embed/src/property_graph/storage/view/source.rs:172-198`, with `storage/allocation.rs:182-183`. `resolve` reserves one logical pathname length, while `artifact_path` simultaneously allocates a formatted `String` and a joined `PathBuf` without measuring/reconciling either actual capacity. It then moves the reservation into `MappedArtifact` after the local `PathBuf` has already dropped. This both omits live peak backing and reports phantom steady-state bytes, contrary to the repository’s exact-capacity/current-accounting rules; accumulated phantom charges can also reject a later lazy open below the real query limit.

- **Medium — production uses denied panic-capable indexing** — `crates/zeppelin-embed/src/property_graph/storage/view/catalog.rs:104-107`. Four `high.symbols[n]` expressions violate the explicit panic-free production rule and the crate-level `deny(clippy::indexing_slicing)`. The indices are presently in bounds, but the documented rule is absolute and a destructuring assignment provides the panic-free form.

## Recorded-evidence finding (local, excluded from the commit)

- **Medium — a “green” artifact records terminal failure** — `tasks/evidence/ze-45/review-physical-split-green.log:4-26` records run `28af3e3c-42a2-4a9a-8bc8-b349a9557f08` as `0 passed, 1 failed` with `Memory`. It cannot support a terminal-GREEN claim. The file is ignored and absent from the product delta; no rerun was made under the user’s prohibition.

## Prior findings and boundaries

The earlier mapping/residency/active-query omission is corrected with live ownership and release accounting. Relationship values are source-bound, construction uses one admitted capability, prepared identity/protection is complete, expansion resume retains physical descriptor position, and adversarial coverage is now receipt/comparator-gated rather than unconditionally filled.

Judgement-only smell: **Low — Divergent Change / Duplicated Code** in the 4,611-line `lifecycle/native_graph.rs`, which combines publication, lifecycle, accounting, preparation protection, fixtures, and probe orchestration with repeated fixture setup.

`Cargo.lock`, dependencies, tracker, inherited `.gitignore`/`AGENTS.md`/`README.md`, and local `.agents`/`CONTEXT.md`/`plan.md`/`skills-lock.json` are absent from the commit range. `git diff --check` is clean.
