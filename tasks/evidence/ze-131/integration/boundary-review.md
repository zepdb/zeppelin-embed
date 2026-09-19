# ZE-131 independent Cargo/source boundary review

## Pin and outcome

- Candidate: `ed7ce4d1dfccc29a2071c66ab449013a2893f7a0`
- Parent: `51f657c60b2bfde991887643126e88e279a75063`
- Original reviewed candidate: `380ea1aed4155e4cdaaac9c50ad5d0f8d82085aa`
- Result: **scoped PASS; no findings in the delegated Cargo/core/registration/probe/header boundary.**

The owner amended `380ea1a` while this review was running. `git diff --name-status
380ea1a ed7ce4d1` contains only target-routing/helper/evidence changes in nine
paths. Every Cargo manifest/lockfile, core cfg file, adversarial module/runner/
coverage/test file, external probe manifest/source, generated header, and symbol
allowlist reviewed here has the same Git blob at both candidates. The one
reviewed probe script changed only by adding a call to the new routing checker;
the full boundary probe was rerun successfully at `ed7ce4d1`. The new routing
checker and the amended CI/coverage/adversarial target-selection logic are owned
by the root routing audit and are explicitly outside this report.

## Standards

No documented-standard breach or material code smell was found in the reviewed
boundary. The feature edges remain explicit and directional; production core
adds no dependency, and the only lockfile edge is the existing internal compiler
package added to FFI's optional dependency set.

- Core declares `graph-cypher` and gates only the native property-graph module
  (`crates/zeppelin-embed/Cargo.toml:18-22`,
  `crates/zeppelin-embed/src/lib.rs:31-37,60-62`). The unsupported-target
  diagnostic is a compile-time error for every non-macOS-arm64 graph build.
- The internal compiler selects core graph and core has no reverse compiler edge
  (`crates/zeppelin-embed-cypher/Cargo.toml:10-11`).
- FFI selects core graph plus the optional internal compiler; text remains an
  independent optional dependency (`crates/zeppelin-embed-ffi/Cargo.toml:12-26`).
- The graph result allocation hook remains a separate explicit feature. The
  module and actual allocation branch are both gated only by
  `graph-result-test-support` (`crates/zeppelin-embed-ffi/src/graph_result.rs:13-15,225-229`).
- The three private lifecycle accessors newly cfg-gated in
  `lifecycle/budget.rs` and `lifecycle/stats.rs` have callers only under
  `src/property_graph/**`; no legacy caller was hidden.
- `Cargo.lock` adds no new package. Its only change is the already-present
  `zeppelin-embed-cypher` package name in the FFI package's dependency list.

## Spec

No missing, partial, wrong, or extra behavior was found in the reviewed scope.

### Standalone products and external probes

The probe manifests are independent workspaces and use path dependencies with
`default-features = false`; they therefore avoid root-workspace feature
unification (`tools/graph-feature-probe/{core,core-graph,ffi}/Cargo.toml`). The
default core probe imports `property_graph` and accepts only the exact missing
module diagnostic; the explicit core probe compiles the same import. Independent
FFI metadata resolves default FFI to core only and graph FFI to core plus the
compiler. It also checks the compiler's only direct internal dependency and the
exact target inventories (`scripts/check-graph-feature-boundary.sh:15-218`).

The existing Node and Python build commands invoke `-p zeppelin-embed-ffi`
without graph features (`bindings/node/scripts/build-native.mjs:71-75,116-128`,
`bindings/python/setup.py:71-82`). Since FFI's default feature set is empty, both
remain legacy products. The FFI external metadata probe independently confirms
that this default selection excludes the compiler.

Exact candidate command:

```text
CARGO_TARGET_DIR=/tmp/ze131-final-boundary-target.AB04k3 \
  bash scripts/check-graph-feature-boundary.sh

graph feature command routing: PASS
default FFI direct dependencies: ['zeppelin_embed']
graph FFI direct dependencies: ['zeppelin_embed', 'zeppelin_embed_cypher']
core graph-required test targets: 21
FFI graph-required test targets: 3
fuzz graph-required targets: 6
graph feature boundary: PASS
exit 0
```

This run compiled default core for installed `x86_64-apple-darwin` and accepted
the graph build failure only after matching `graph-cypher requires macOS arm64`;
it did not count a missing standard library or linker error.

### Core, FFI, and fuzz test targets

The 21 top-level core integration sources that actually import
`zeppelin_embed::property_graph` are exactly the 21 manifest targets requiring
`graph-cypher` (`crates/zeppelin-embed/Cargo.toml:68-150`). The legacy ANN
`graph_node_blocks` source has no native property-graph import and remains a
default target. Independent focused execution at the candidate passed all 10
legacy tests:

```text
CARGO_TARGET_DIR=/tmp/ze131-final-runner-target.yICLsU \
  cargo test -p zeppelin-embed --test graph_node_blocks
PASS: 10 passed, 0 failed
```

FFI's three graph contract/header/layout targets require `graph-cypher`
(`crates/zeppelin-embed-ffi/Cargo.toml:36-46`). The excluded fuzz workspace makes
the compiler optional and requires `graph-cypher` on exactly the six targets
that import compiler/native graph code, while legacy ANN/parser-independent
targets stay default (`fuzz/Cargo.toml:10-23,119-165`).

### Adversarial modules, calls, and keys

Every native graph module declaration and every corresponding runner call is
behind `graph-cypher`; `graph_response` alone is behind the narrower
`graph-result-test-support` feature (`tests/adversarial/mod.rs:11-133`,
`tests/adversarial/runner.rs:2874-2909`). Normalizing the runner calls yields the
same exact 17-call set before and after the change, including
`property_graph_storage`; no runner body or probe call was deleted.

The parent required ledger partitions exactly into 88 legacy keys and 159 graph
keys at the candidate, with no set difference. The 159 include 12 response-hook
keys, so ordinary graph has `88 + 147 = 235` active keys and the hook build has
247 (`tests/adversarial/coverage.rs:3-281`). The full source inventory is also
unchanged:

```text
unique quoted property-graph.* strings, parent:    162
unique quoted property-graph.* strings, candidate: 162
sorted SHA-256, both:
18003186cea3ca1c141a527a1f015da5e65c603bee6521ada02b3b2b7d9198eb
set difference: 0
```

To check actual receipts rather than list counts, I added one temporary test
only to the extracted `/tmp/ze131-sol-final.TbI16q` archive. It runs one real
runner episode, asserts zero violations, checks the emitted coverage registry,
and is not part of the candidate. All three exact-candidate source selections
passed:

```text
CARGO_TARGET_DIR=/tmp/ze131-final-runner-target.yICLsU \
  cargo test -p zeppelin-embed-workspace-tests --test ze131_review_probe \
  runner_earns_exact_active_graph_ledger -- --exact --nocapture
ZE131_ACTIVE_REQUIRED_KEYS=88; PASS

CARGO_TARGET_DIR=/tmp/ze131-final-runner-target.yICLsU \
  cargo test -p zeppelin-embed-workspace-tests --features graph-cypher \
  --test ze131_review_probe runner_earns_exact_active_graph_ledger \
  -- --exact --nocapture
ZE131_ACTIVE_REQUIRED_KEYS=235; every required native key earned; PASS

CARGO_TARGET_DIR=/tmp/ze131-final-runner-target.yICLsU \
  cargo test -p zeppelin-embed-workspace-tests \
  --features graph-result-test-support --test ze131_review_probe \
  runner_earns_exact_active_graph_ledger -- --exact --nocapture
ZE131_ACTIVE_REQUIRED_KEYS=247; all 12 response keys earned; PASS
```

The default receipt JSON contains no `property-graph.` key. The ordinary graph
receipt JSON contains no `property-graph.response.` key. Thus module selection,
runner calls, required ledgers, and actual earned receipts agree in all three
modes.

### Header and public-runtime separation

The legacy generated header, separate graph data header, and legacy symbol
allowlist are byte-identical to the parent. The graph header contains data
contracts and no runtime function declaration; neither the legacy header nor
the allowlist contains `ze_graph_`. The FFI source diff adds no C function or
stub. Focused exact-candidate checks passed:

```text
cargo test -p zeppelin-embed-ffi --features graph-cypher \
  --test ffi_graph_header legacy_header_and_exports_do_not_advertise_graph_contracts \
  -- --exact
PASS: 1

cargo test -p zeppelin-embed-ffi --features graph-cypher \
  --test ffi_graph_header graph_contract_header_is_exact_separate_cbindgen_output \
  -- --exact
PASS: 1
```

SHA-256 from the extracted candidate:

```text
zeppelin_embed.h:            7f54da78bd1422e13ae3285232649a7b564f8319a5840ccc434aa5566bea9b32
zeppelin_graph_contracts.h:  93e7aec6339cf80747df7810584b25f2da235a2fe0e586cd1c02b59e146b34f8
symbols.allowlist:            4ada26f510a9a07e01200a12e13f2fb324109d75db18665f161a22aace55c440
```

## Review limits

This is compiled feature-boundary and focused registration/header evidence. It
does not qualify linked compiler reachability, graph artifact size, public graph
runtime functions, SDK/XCFramework/Swift packaging, installed consumers,
minimum macOS 14 runtime, publishing, or release footprint. Those remain with
ZE-107/69/71/78. I did not run broad workspace, adversarial, coverage, fuzz, or
size campaigns. The uncommitted ZE-44 target/runner integration and the ZE-132
fixture-helper correction are outside this candidate and were not reviewed.
The known PID-only same-process Cypher fixture collision was not treated as a
ZE-131 defect.

The new target-routing/helper implementation added between `380ea1a` and
`ed7ce4d1` was not independently reviewed here; the root reviewer owns that
audit. Its invocation completed successfully as part of the final boundary
probe, but that execution is not substituted for the root's routing review.
