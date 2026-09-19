# ZE-35 — graph catalog and interpretation participant

## Scope and reviewed design

Base: `a3e093348ba03e42218613bda598daf2c2c061b2`; isolated branch
`codex/ze-35-graph-catalog`. Hardware: Apple M3 Max, 128 GiB, native arm64
macOS 27.0 build 26A5388g. Rust 1.93.0, LLVM 21.1.8,
cargo-llvm-cov 0.9.0. Exact environment is in `ze-35/environment.json`.
No third-party dependency, lockfile change, or global persisted family was added.

The coordinator accepted the concrete ZGCA v1 logical participant layout before
encoding work: 120-byte prefix, complete StoreInstanceId, full logical and symbol
high-waters, TokenizerEpoch, optional complete document tower, exact symbol rows,
and xxh3-64 trailer. The tracked core guide records exact field order and tags.
Independent Python struct assembly pins the 237-byte no-space and 314-byte
with-document goldens. `ze-35/generate_goldens.py` verifies them without using the
production codec and also generates the 131,227-byte split-codepoint fuzz seed.

The public seam uses four distinct nonzero symbol types, exact UTF-8 names
(including empty/NUL), stable assignments within independent domains, preserved
high-water gaps/exhaustion, and zero or one document space. It requires no vector,
text, or timestamp membership. Document interpretation includes every tower field;
query-tower/alignment changes do not change document identity. Pure controlled
admission rejects changed store/interpretation before a later coordinator may
proceed. It has no VFS, entropy, cleanup, replay, or publisher authority.

This ticket proves logical reconstruction/relocation and declaration admission.
It does **not** prove whole GraphStore reopen/compaction or runtime replay/cleanup
ordering; those remain ZE-38/40/43 responsibilities. The bounded dictionary is an
optional construction/codec helper, not a mandatory resident or copy-per-write
catalog. Later paged storage can supply records without using it.

## Resource and cancellation contract

Borrowed strings/tower/input buffers retain caller/lease ownership and accounting.
Descriptor capacity is fixed, reserved fallibly against a caller allowance already
reserved from the shared budget, and checked against the actual Vec capacity.
No interning allocation or descriptor shifting follows reservation. Complete
encoded framing is checked before touching the caller's output on size refusal.
Cancellation may leave a private output prefix; that is not publication.

Descriptor work/sorting and exact byte comparisons poll cancellation. Comparisons,
UTF-8 validation, checksum work and output copies process at most 64 KiB per byte
checkpoint. Incomplete UTF-8 codepoints carry into the next validation chunk.
The sole unchecked string conversion follows complete immutable-byte validation;
public tests exercise all three split positions and invalid continuations.
Derived equality is a convenience, not the controlled admission entrypoint.

Independent review found deferred cancellation in initial sort/name/admission
comparisons. Named tests observed the actual missed cancellation before the fix.
The coordinator reviewed the corrected four-file snapshot and found no remaining
concrete defect; reviewed/restored hashes are recorded in `ze-35/mutations.json`.

## Literal RED and terminal GREEN

Every RED below is a failing assertion/typed-result mismatch, not compilation.
Raw logs live under `ze-35/`, losslessly gzip-compressed, with corresponding
`red-`/`green-` names. This preserves original terminal whitespace and bytes.

| Named test | Observed RED | Terminal proof |
| --- | --- | --- |
| `catalog_symbols_preserve_full_width_and_refuse_zero_or_overflow` | maximum symbol incorrectly returned ZeroSymbol | green-symbols; final-core-contracts |
| `catalog_reconstruction_preserves_exact_names_domains_and_high_waters` | retained label lookup returned None | green-symbol-catalog; final-core-contracts |
| `catalog_rejects_duplicate_assignments_and_regressed_high_waters` | duplicate accepted | green-symbol-catalog; final-core-contracts |
| `catalog_admission_refuses_every_changed_document_field_or_analyzer` | changed document admitted | green-admission; final-core-contracts |
| `logical_catalog_roundtrip_preserves_store_symbols_counters_and_optional_space` | codec stub returned Malformed | green-roundtrip; final-core-contracts |
| `catalog_descriptor_reservation_accounts_every_retained_byte` | 256 allocated bytes were unattributed | green-allocation; final-core-contracts |
| `catalog_long_name_lookup_observes_cancellation_during_comparison` and `catalog_long_interpretation_observes_cancellation_during_exact_comparison` | returned success after cancellation should fire | green-long-cancellation; final-core-contracts |
| `property_graph_catalog_faults_fire_with_same_seed_clean_controls` | cancel.fire count 0 instead of 8 | green-fault-controls |

`green-reconstruction.log.gz` is retained as an intermediate run that also exposed
the then-unfixed duplicate test; it is **not** terminal GREEN evidence.
The final public suite has 14 passing tests plus the feature-gated allocation test.

Eight isolated behavioral mutations were detected: disabled duplicate ID rejection,
regressed high-water admission, truncated store identity, omitted model-version
comparison, missing byte-chunk cancellation poll, admitted reserved bytes, broken
UTF-8 carry, and blind independent dictionary oracle. Each named test failed;
`mutation_proof.py` restored exact original bytes in `finally` and verified SHA256.
Final contract/oracle reruns are the restored-source GREEN, not the mutation runs.

## Independent oracle and adversarial evidence

PG4 models primitive names, IDs, counters and interpretation facts with standard
maps/sets, without importing engine code. Its external test rejects malformed and
mismatched observations as well as accepting correct controls. The seeded runner
probe uses the project's named `seeded_rng`, eight rounds per invocation, and
required keys for exact names, duplicates, exhaustion, interpretation, store
identity, cancellation, budget and checksum faults.

The fault campaign records eight firings and eight same-seed clean controls for
each of cancellation, insufficient descriptor allowance, and checksum corruption.
Cancellation is scheduled halfway through a measured successful decode traversal;
it must fire exactly once and stop at that checkpoint. Clean controls preserve
full declarations, rows and high-waters. The primitive interpretation campaign
varies model ID/dimension/presence; exhaustive complete-tower field checks remain
at the public core seam, not claimed as primitive campaign coverage.

Pre-change full adversarial matrix: **426 passed, 0 failed, 11 ignored** in
2437.58 seconds, including the seeded smoke. The already-built baseline executable
ran while independent feature code was developed. Restored-source post-change
smoke: **224 seeds (0..223), 0 violations**, passed in 128.11 seconds. Both added
PG4 probe tests pass. This distinguishes the full baseline matrix from the
post-change seeded smoke; it does not claim a repeated full matrix after the change.

## Commands and measured gates

Commands ran from the isolated worktree. Set `CARGO_TARGET_DIR` to its absolute
`target` directory for normal tests, `target-doc` for rustdoc, `target-fuzz` for
fuzz, or `target-coverage` for the supplemental coverage commands. Main targets
and retained main profiles were never used.

```sh
cargo test -p zeppelin-embed --features allocation-audit --lib --test graph_catalog catalog_ -- --test-threads=1
cargo test -p zeppelin-embed-adversarial-oracle --test graph_catalog
cargo test -p zeppelin-embed-workspace-tests --test adversarial_tests property_graph_catalog_ -- --nocapture
cargo test -p zeppelin-embed-workspace-tests --test adversarial_tests
cargo test -p zeppelin-embed-workspace-tests --test adversarial_tests smoke -- --exact --nocapture
cargo fmt --all -- --check
cargo clippy -p zeppelin-embed -p zeppelin-embed-adversarial-oracle -p zeppelin-embed-workspace-tests --all-targets -- -D warnings
RUSTDOCFLAGS='-D warnings' cargo doc -p zeppelin-embed --no-deps
cargo deny check
scripts/size-budget.sh
cargo fuzz run graph_catalog --debug-assertions -- -max_total_time=60 -max_len=131072
cargo fuzz run graph_catalog --debug-assertions -- -max_total_time=60 -max_len=262144
```

Formatting, warnings-denied Clippy/rustdoc and dependency policy pass. Existing
cargo-deny advisory/license exceptions remain warnings as recorded; no policy was
weakened. Native arm64 linked static-library size: core **3645 KB**, FFI **3840 KB**,
both under **5120 KB**. Physical archives: 23408/24468 KB. Outer text: 21216 KB
linked/56456 KB archive, reported only; minimal consumer: 1010 KB linked/1060 KB
physical, reported only. These are native arm64 measurements, not Intel or Windows
qualification. Full raw size output is retained.

Initial raw-byte fuzz run: **861,901 executions in 61 seconds**, no sanitizer
finding. Expanded harness keeps that raw leg and adds outer length/checksum repair
to reach semantic parsing after mutation; its final run completed **548,869 executions in 61 seconds**, with no sanitizer finding. The
large seed crosses the UTF-8 chunk boundary and fits max_len 262144. Fuzz smoke
is bounded evidence, not exhaustive parser proof. The first bare fuzz invocation
hit the existing macOS Bash3 empty-array wrapper bug; `--debug-assertions` supplies
the already-enabled default option explicitly. No wrapper code was changed.

Supplemental focused LLVM instrumentation:

```sh
cargo llvm-cov test --no-report -p zeppelin-embed --features allocation-audit --lib --test graph_catalog catalog_ -- --test-threads=1
cargo llvm-cov test --no-report -p zeppelin-embed-adversarial-oracle --test graph_catalog
cargo llvm-cov report --no-default-ignore-filename-regex --ignore-filename-regex '(^|/)(registry/|crates/zeppelin-embed-bench|fuzz/|target/|rustc/)|/\.rustup/' --json --output-path /tmp/ze-35-focused.json
```

Actual LLVM file line totals: catalog 226/236, codec 260/269, interpretation 126/126,
work 91/93: **703/724 = 97.0994%** for these four files. Independent PG4 oracle:
**72/72 = 100%**. Exact inventory/line counters/hashes are in
`ze-35/focused-coverage.json`; no test-driver totals are counted in the oracle
subtotal. This focused report is supplemental, **not** the full per-crate gate.
The focused implementation is integrated below. The owner subsequently deferred nonessential broad final-source qualification to ZE-118.

## Main integration and owner-directed qualification scheduling

Source commit `23fb6b961087281fbba90934966347d7231fa0aa` was cherry-picked alongside ZE-33/42, retaining all shared modules, probes, coverage keys, fuzz targets and guidance. Catalog production code matches the reviewed worker hashes. Core/oracle declarations are appended after the complete preexisting file bytes, preserving executable source positions. The exact final audit is `ze-35/integration/final-source-and-nextest.json`.

Before the owner's scheduling change, integration ran 584 core library tests (2 existing ignores) with allocation-audit enabled, 14 catalog tests, independent PG4 tests, combined probes and a 224-episode smoke with zero violations; strict Clippy/fmt passed. The initial narrower allocation-feature run exposed additional existing feature-gated paths and core was 89.92%; the full allocation-audit library run exercised those paths without changing production code or thresholds. Actual intermediate report: core 64,684/71,620 = 90.315554%, Cypher 1,514/1,554 = 97.425997%, FFI 3,484/3,867 = 90.095681%, text 3,245/3,550 = 91.408451%, oracle 14,842/16,394 = 90.533122%. Full raw reports, the earlier report and exact commands are archived. These reports precede the final declaration-only oracle EOF move; they are retained measured evidence, not a claim of a repeated final-source full campaign.

Final-source nextest execution passed all 7 selected independent oracle and combined property-graph probe tests, with 435 deliberately unselected tests and zero failures; catalog cancellation, budget and decode-error controls each recorded 8 fault firings and 8 clean controls. Final formatting passes. Existing 14 public catalog cases and allocation audit remain unchanged and previously passed on integrated source; the nextest installation also ran all 14 public catalog cases successfully. No semantic source changed after those checks.

The owner instructed that nonessential full/adversarial/coverage runs wait until implementation is complete. **ZE-118, E12 Backlog**, owns final integrated workspace/adversarial/per-crate coverage qualification; it blocks final release, not this verified component implementation. Do not interpret focused GREEN as whole GraphStore lifecycle, final release, minimum-macOS or other-platform evidence.
