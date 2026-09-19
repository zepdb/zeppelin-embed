# ZE-128 PG16 seeded runner followup

This is a separate followup to initial candidate
`f1c4d20dd4086a77ca7fdfbe7276311ea796833a`. The initial aligned-owner evidence
remains in the parent directory. Host/tool versions are unchanged from
`../host.txt`: Apple M3 Max arm64, 128 GiB, Rust 1.93.0, nextest 0.9.145.
Source hashes for all twelve changed/new source and manifest paths are in
`frozen-source-inventory.json`. No mutation remains.

## Actual route and independent checks

PG16 runs through the existing `run_program` route, with twelve required
coverage keys in `tests/adversarial/coverage.rs`. It calls the compiled public
Rust `GraphResultRegistry` prepare/expose/free and `OutcomeCell` methods.
The independent oracle receives only primitive observations; it imports no
engine owner/layout/C descriptor helpers. The seeded generator uses the existing
adversarial `test_support::seeded_rng` entrypoint. Four directed seeds are 0, 1,
128 and u64::MAX. Each reports 16 cases, six fault fires and six clean controls.
The actual canonical runner episode at seed 128 reports 58 operations, zero
violations and every PG16 coverage key present.

The probe checks exact integer/UTF-8/NUL payloads, root counts, observed alignment,
private/forged/stale/second-free handling, abort refund, publish versus free,
two concurrent free contenders with one owner, terminal known generation after
unwind and rejected downgrade. Each fault is followed by a complete clean
prepare/expose/free using identical source bytes and seed. Actual existing
query accounting must return to its baseline. Allocation faults have explicit
matching-site/fire receipts; cancellation records actual triggered cancellation;
work, memory and registry controls require their exact typed refusal and clean
completion. They are not randomized scheduler coverage or an exhaustive campaign.

The two allocation sites are the actual arena and registry-node allocation calls.
The opt-in controller has const-initialized thread-local Cell state, no heap or
hidden mutex, and RAII restoration including unwind. A requested ordinal alone
is not evidence: the probe and independent oracle require actual receipt counts.
This site injection returns null through the existing typed Allocation path.
It is **not** new system-allocator-denial evidence. The original actual System
allocator denial, no-allocation expose/free and exact heap loops remain covered
by the thirteen component tests, rerun here with the hook compiled but unarmed.

The source pools are synthetic real-owned typed C records. The runtime uses real
Store accounting and SnapshotLease but a fixture QueryView token. No authentic
GraphStore admission, native-result conversion, actual commit or C runtime export
is claimed. Known outcome evidence exercises the compiled cell, not a fabricated
commit. ZE-68/69 still require real source ownership/complete native-and-C overlap,
true commit-window denial, actual coordinator outcomes and public free/Busy/Poison
policy, graph close/reopen and lifecycle acceptance.

## Feature isolation

The workspace-test `graph-cypher` feature enables the FFI
`graph-result-test-support` feature, which in turn enables `graph-cypher`.
Only that explicit test feature compiles the controller. Default builds omit the
graph owner/module, hook, route and twelve keys. Shipping `graph-cypher` alone
compiles the existing graph component without the controller. No shipping C
export, dependency or header is introduced. The independent primitive oracle
remains ordinary test-tooling code with no product hook.

`15-feature-matrix.json` records actual locked/offline cargo metadata resolution:
workspace default has only FFI `abi-panic-probe, default`; explicit shipping
FFI graph adds only `graph-cypher`; opt-in workspace runner adds both graph and
`graph-result-test-support`. The preexisting `abi-panic-probe` comes from the
workspace test dependency, not this followup. Direct default and graph-only FFI
compilation pass, as does the default runner test proving all PG16 keys absent.

## Observed RED, restoration and terminal GREEN

Readable logs normalize trailing whitespace; `raw-terminal-logs.tar.gz` retains
byte-exact output, with original hashes in `raw-terminal-inventory.json`.

- `01-before-runner.log`: old actual runner binding episode passes before the
  new route and feature are added (one passed, 455 filtered).
- `02-missing-route-red.log`: the named new probe fails to compile because its
  actual runner module does not exist yet, exit 101.
- `03-probe-check.log`: retained fixture error, not product RED or GREEN. The
  fixture originally armed cancellation before RuntimeContext construction's
  own checks. Arming after construction fixes its intended operation boundary.
- `04-probe-green.log`: four seeded direct probes pass after that fixture fix.
- `05-missing_route-red.log`: removing the real runner call makes the actual
  episode test reject missing `property-graph.response.aligned-owner`, exit 100.
- `05-missing_fire-red.log`: suppressing the actual allocation-fire receipt while
  retaining the refusal makes the primitive oracle reject unproved Allocation(1),
  exit 100. `mutations.json` records both exact edits, commands and byte-identical
  original/restored hashes; `mutations.py` restores in finally blocks.
- `06-restored-green.log`: two focused runner tests pass, 456 filtered; includes
  all four directed seed receipts and the actual runner episode described above.
- `07-ffi-clippy.log`, `13-runner-clippy.log`, `14-oracle-clippy.log`: strict
  scoped checks pass without changing or suppressing inherited dependency lints.
- `08-feature-off.log`: default feature-off test passes, 456 filtered.
- `09-owner-regression.log`: thirteen owner tests pass, fourteen filtered.
- `10-oracle.log`: independent oracle negative-control test passes, 92 filtered.
- `11-default-ffi.log`, `12-shipping-graph.log`: direct feature matrix compile passes.
- `16-fmt.log`: all three changed Rust packages pass formatter check (empty output).

Exact commands, from the assigned worktree; nextest uses the existing profile
with four isolated test processes, one libtest thread each and no retries:

```sh
# Before adding the route, hook and workspace feature:
cargo nextest run -p zeppelin-embed-workspace-tests --features zeppelin-embed-ffi/graph-cypher --test adversarial_tests -E 'test(one_runner_episode_reaches_required_binding_contracts)'
# Restored terminal focused runner checks:
cargo nextest run -p zeppelin-embed-workspace-tests --features graph-cypher --test adversarial_tests -E 'test(property_graph_response_probe_checks) | test(one_runner_episode_reaches_required_graph_response_contracts)' --success-output immediate
cargo nextest run -p zeppelin-embed-workspace-tests --test adversarial_tests -E 'test(graph_response_runner_keys_are_absent_without_graph_feature)'
cargo nextest run -p zeppelin-embed-ffi --features graph-result-test-support --lib -E 'test(graph_result_)'
cargo nextest run -p zeppelin-embed-adversarial-oracle --lib -E 'test(graph_response_oracle)'
cargo check -p zeppelin-embed-ffi --no-default-features --lib
cargo check -p zeppelin-embed-ffi --no-default-features --features graph-cypher --lib
cargo clippy -p zeppelin-embed-ffi --features graph-result-test-support --lib --tests --no-deps -- -D warnings
cargo clippy -p zeppelin-embed-workspace-tests --features graph-cypher --test adversarial_tests --no-deps -- -D warnings
cargo clippy -p zeppelin-embed-adversarial-oracle --lib --no-deps -- -D warnings
cargo fmt --package zeppelin-embed-ffi --package zeppelin-embed-workspace-tests --package zeppelin-embed-adversarial-oracle --check
```

This focused actual changed-path runner check is mandatory ZE-128 verification,
not deferred qualification. Full workspace/adversarial campaigns, final complete
crate coverage, release archive/platform/packaging suites remain ZE-118. The two
original ignored release archive cases are unchanged and not claimed passing.

## Independent review

Root delegated a bounded followup review to /root/ze119_capacity. No concrete
blocker was found; all twelve source hashes and both mutation restorations
were verified. The reviewer independently compiled the frozen std-only oracle
and ran its one exact negative-control test (one pass), while explicitly
distinguishing inspected owner runner logs from independent reruns. See
independent-review.md, independent-checks.json and independent-oracle.log.
Root also read the controller, primitive oracle, probe and feature diff and
reported no blocker. All real producer/public integration boundaries remain.
