# ZE-45 integration correction

Base main: `75dafc3182ae815a19186632490d8a69c1d85c84`.
Environment: same Apple arm64 host/toolchain recorded in `../environment.txt`.
No dataset or adversarial episode was executed in these compile checks.

Three integrated feature-boundary errors were reproduced and corrected:

- Without graph support, `Store::stats` referenced the feature-gated
  `native_graph` field (E0609). Graph mapping statistics now use the same
  feature guard, with zero native mappings/residency in builds without it.
- The cherry-pick conflict resolution omitted the `graph-cypher` guard on
  the read-view runner module. After correcting stats, the same compile
  command exposed E0432/E0433. The module guard is restored independently
  of the adjacent mutation-lowering module.
- With `graph-result-test-support`, the conversion diagnostic match omitted
  ZE-45's `RuntimeError::IdentityExhausted` (E0004). It now explicitly maps
  to the existing Other diagnostic, keeping the failure exhaustive.

Raw commands and terminal exit codes are in the adjacent logs. Compile-only
checks passed for the selected adversarial test target with no graph feature,
`graph-cypher`, and `graph-result-test-support`. This is compilation evidence,
not an adversarial runner execution. Source inspection confirmed 217 unique
registry keys with graph-only and 237 with result test support.

Two existing default-feature statistics tests passed under isolated nextest,
run `d6ab2442-39f8-4b8e-a7df-2c1c47a77c05`: `stats_bytes_are_conserved` and
`repeated_close_returns_exact_stats_counters_to_pre_open_baseline`.
568 tests were skipped. `git diff --check` passed.

The preceding integrated 25-test native run remains recorded in the ticket:
`ff189738-fb32-453c-a002-323114c6bfab`. The correction leaves that graph
statistics path unchanged. Broad/adversarial suites remain deferred to ZE-118.
