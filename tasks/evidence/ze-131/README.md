# ZE-131 opt-in graph feature boundary

Status: focused GREEN. This is compiled Cargo/module/test-registration evidence,
not graph artifact, public runtime, installed-consumer, minimum-macOS, or
footprint qualification.

## Source and host

- Baseline: `51f657c60b2bfde991887643126e88e279a75063`
- Branch: `codex/ze-131-graph-features`
- Host: MacBook Pro arm64, macOS 27.0 build 26A5388g, Darwin 27.0.0
- Rust: rustc 1.93.0 (254b59607d4417e9dffbc307138ae5c86280fe4c),
  Cargo 1.93.0, LLVM 21.1.8
- Tools: cargo-nextest 0.9.145, cbindgen 0.29.4

No new third-party package entered either lockfile. The root `Cargo.lock` change
adds only the existing internal `zeppelin-embed-cypher` package to the FFI
package's optional dependency edges.

## Literal RED to GREEN

The first durable probe was added before changing production features:

```text
$ bash scripts/check-graph-feature-boundary.sh
default core consumer unexpectedly imported property_graph
exit 1
```

This is the intended RED: the isolated default external core consumer could
still import the native property-graph module. It completed in 4.48 seconds.

After the feature boundary was implemented, the same command passed:

```text
$ bash scripts/check-graph-feature-boundary.sh
graph feature command routing: PASS
default FFI direct dependencies: ['zeppelin_embed']
graph FFI direct dependencies: ['zeppelin_embed', 'zeppelin_embed_cypher']
core graph-required test targets: 21
FFI graph-required test targets: 3
fuzz graph-required targets: 6
graph feature boundary: PASS
exit 0
```

The probe uses separate external manifests and target directories for default
and graph-enabled core selection. It also resolves default and graph FFI
metadata independently. It verifies that the compiler's only internal direct
dependency is core, and that core never selects the compiler. This proves
dependency selection, not linked compiler reachability or artifact footprint.

The same probe compiled default core for the installed
`x86_64-apple-darwin` standard-library target and observed the graph selection
fail on the exact intentional diagnostic `graph-cypher requires macOS arm64`.
No missing standard library or linker failure was counted. This is a real
cross-compile boundary check, not Intel runtime or minimum-macOS evidence.

## Qualification routing RED to GREEN

Independent review of the first frozen candidate, commit `380ea1a`, used a
Cargo stub that recorded arguments and exited 73 before any build. Seven of 12
routes were wrong: all three wrappers selected graph on an Intel Mac, the
adversarial wrapper selected graph on Linux, and all three wrappers selected
graph on an arm64 Mac whose explicit Cargo target was
`x86_64-apple-darwin`. The exact RED rows are in `routing-red.json`.

The wrappers now select native graph qualification only when both the native
host is macOS arm64 and the effective Cargo target is
`aarch64-apple-darwin`. Target resolution uses, in precedence order, a
command-line `--target` where the wrapper accepts Cargo arguments,
`CARGO_BUILD_TARGET`, then the selected toolchain's `rustc -vV` host triple.
Every unselected path reports that it is running legacy qualification. The
same independent 12-route harness passed all 12 rows; the exact GREEN rows are
in `routing-green.json`.

The repository-owned routing probe also covers Linux and Windows adversarial
selection, an arm64 native host with an x86_64 Rust toolchain, and command-line
target precedence:

```text
$ scripts/check-graph-feature-routing.sh
graph feature command routing: PASS
```

Both routing harnesses stop at a stubbed Cargo command. They verify command
selection only; no broad CI, coverage, or adversarial suite ran.

## Feature and registration inventory

Core has 21 integration targets whose source imports native
`zeppelin_embed::property_graph`; every one requires `graph-cypher`. The legacy
ANN `graph_node_blocks` target remains enabled without it. The excluded fuzz
workspace has six graph/compiler targets requiring `graph-cypher`; its legacy
targets remain default. FFI has three graph contract/header/layout targets
requiring the feature.

Workspace adversarial selection is split into:

| Selection | Active required keys | Meaning |
|---|---:|---|
| default | 88 | legacy campaigns, including ANN `search.graph` and `search.filtered_graph` |
| `graph-cypher` | 235 | 88 legacy plus 147 native graph keys; PG16 hook absent |
| `graph-result-test-support` | 247 | ordinary graph plus 12 PG16 allocation-hook keys |

The complete native graph key source inventory was preserved. The 162 unique
quoted `property-graph.*` strings under `tests/adversarial` have the same sorted
SHA-256 before and after:

```text
18003186cea3ca1c141a527a1f015da5e65c603bee6521ada02b3b2b7d9198eb
```

The 162 source strings include diagnostic and module-local keys outside the 159
native smoke-required entries. The enabled ordinary/test-hook inventory tests
prove all selected required keys are active, while the default test proves no
`property-graph.*` key is active and the legacy ANN keys remain present.

## Focused verification

Compilation and matrix checks:

```text
cargo check -p zeppelin-embed-workspace-tests --tests
  PASS
cargo check -p zeppelin-embed-workspace-tests --tests \
  --features graph-result-test-support
  PASS
cargo check --manifest-path fuzz/Cargo.toml --all-targets
  PASS
cargo check --manifest-path fuzz/Cargo.toml --all-targets \
  --features graph-cypher
  PASS
cargo test -p zeppelin-embed --features graph-cypher --tests --no-run
  PASS; all 21 graph-required targets compiled
```

Registration and focused runtime contracts:

```text
cargo test -p zeppelin-embed-workspace-tests --test adversarial_tests \
  native_graph_runner_keys_are_absent_without_graph_feature -- --exact
  PASS: 1
cargo test -p zeppelin-embed-workspace-tests --features graph-cypher \
  --test adversarial_tests runner_keys_are -- --nocapture
  PASS: 2
cargo test -p zeppelin-embed-workspace-tests \
  --features graph-result-test-support --test adversarial_tests \
  graph_response_runner_keys_are_active_with_test_hook -- --exact
  PASS: 1
cargo test -p zeppelin-embed-workspace-tests \
  --features graph-result-test-support --test adversarial_tests \
  property_graph_response_probe_checks_real_owners_and_paired_faults \
  -- --exact --nocapture
  PASS: 1; four seeds, each 16 cases / 6 fires / 6 clean controls
cargo test -p zeppelin-embed --features graph-cypher \
  --test graph_query_runtime \
  runtime_counters_enforce_limits_before_work_and_keep_one_view_control \
  -- --exact
  PASS: 1
cargo test -p zeppelin-embed-ffi --features graph-cypher \
  --test ffi_graph_contract --test ffi_graph_header --test ffi_graph_layout
  PASS: 21 (10 contract, 9 header/consumer/drift, 2 layout)
cargo test -p zeppelin-embed --test graph_node_blocks
  PASS: 10 legacy ANN tests without graph-cypher
cargo test -p zeppelin-embed-workspace-tests --test adversarial_cli
  PASS: 6; stable list and pre-Cargo CLI validation remain unchanged
```

The compiler runtime-lowering target uses a PID-based fixture directory. Plain
libtest ran its four cases concurrently in one process and produced three
`AlreadyExists` failures at `runtime_lowering.rs:20`; one case passed. This is
retained as a diagnostic, not a product RED. The same four cases passed both
single-threaded under libtest and concurrently as separate nextest processes:

```text
cargo test -p zeppelin-embed-cypher --test runtime_lowering \
  -- --test-threads=1
  PASS: 4
cargo nextest run -p zeppelin-embed-cypher --test runtime_lowering -j 4
  PASS: 4, run 09a8b089-90dd-43c1-9094-d5c601efb406, 0.573s
```

Strict formatting and lint:

```text
cargo fmt --all -- --check
  PASS
cargo clippy -p zeppelin-embed --all-targets -- -D warnings
  PASS
cargo clippy -p zeppelin-embed --all-targets \
  --features graph-cypher -- -D warnings
  PASS
cargo clippy --workspace --all-targets \
  --features zeppelin-embed-workspace-tests/graph-result-test-support \
  -- -D warnings
  PASS in 1m04s
cargo clippy -p zeppelin-embed-ffi --all-targets \
  --features graph-cypher --no-deps -- -D warnings
cargo clippy -p zeppelin-embed-cypher --all-targets --no-deps -- -D warnings
cargo clippy -p zeppelin-embed-workspace-tests --all-targets \
  --features graph-result-test-support --no-deps -- -D warnings
  PASS
bash -n scripts/check-graph-feature-boundary.sh \
  scripts/check-graph-feature-routing.sh scripts/graph-feature-target.sh \
  scripts/adversarial.sh scripts/cargo-fuzz-nightly scripts/ci-gates.sh \
  scripts/coverage.sh
  PASS
git diff --check
  PASS
```

An isolated FFI graph lint without `--no-deps` exposed pre-existing core
warnings in kernel fault-hook/lifecycle code when core test-support feature
unification is absent. ZE-131 did not edit those unrelated engine paths. The
exact workspace strict lint, core default/graph lint, and package-seam strict
lint all pass.

## Qualification boundary

No full workspace suite, adversarial smoke/campaign, coverage run, fuzz run,
release build, size measurement, package build, SDK/XCFramework/Swift run, or
installed consumer ran here. ZE-118 owns broad qualification. ZE-107 still
owns real graph artifact/header/export identities, compiler reachability and
early graph-plus-compiler-plus-consumer section measurements. ZE-69/71/78 still
own actual public runtime, installed consumers, native macOS 14 arm64, and the
final 5,120 KiB shipping gates.
