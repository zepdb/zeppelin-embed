# Step17 raw execution evidence

Exact commands and process exits are in adjacent JSON receipts. `.log.gz` files
preserve original stdout/stderr byte-for-byte; uncompressed originals remain in
the working artifact directory. Cargo exit101 is the intended assertion RED
only for the reuse/admission RED and deliberate fault-plant receipts. The first
admission GREEN attempt instead records a compile error; its subsequent retry
and final nine-test runs pass. No compile failure is counted as behavioral RED.

`directed-plants.json` records reservation and field-key assertion failures and
the restored clean controls. `fast-path-checks.json` records the final six unit
and three public tests. `checkpoint.json` records five selected integration
cases. `final-clippy.json` records the final lint command and exit.

The original all-key cache screen, singleton-admission screen and final no-cache
fast-path screen are separate directories, including their negative controls.
Timing binaries have no test-support; deterministic observers are test-only.
