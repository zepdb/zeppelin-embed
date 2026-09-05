# Preserved scan and embedding investigation

`follow-up.patch.gz` contains experimental telemetry, controls and the extended harness on top
of the pre-existing clean71 worktree at detached cf312af. It is archived here
so committing the work does not change that worktree HEAD or ship diagnostic
QoS/worker controls in the application. It requires that worktree's custom
model/tokenizer adapters and the first-wave output-copy changes. Do not apply
it blindly to main or to the already modified experimental worktree.

`initial-state.json` pins the inputs; `final-source-hashes.json` pins the changed
files. Source copies and raw runs are in
`/private/tmp/ze-query-investigate-vci05npf`. The report and experiment register
are in `tasks/evidence/query-api-scan-embedding.md`. Manifest argv/environment
fields are exact receipts; use fresh output directories when replaying. The
standalone main manifest at `../Cargo.toml` is the retained general change.

Inspect with `gzip -dc follow-up.patch.gz`; compression preserves blank patch
context lines without repository whitespace-check false positives.
