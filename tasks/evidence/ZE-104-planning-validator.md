# ZE-104: restore graph planning validation

Verified 2026-09-18 on Apple M3 Max (Mac15,9), 128 GiB RAM,
macOS 27.0 build 26A5388g, Python 3.9.6, Node v24.21.0.
Worktree `codex/ze-104-planning-validator` starts at
`bf13bc11239c8161a389d3ce26a43b97412847b8`.

The tracked validator and its tests are in `scripts/planning/`. It reads
local planning artifacts and one current tracker export, without importing
or committing private tracker data. The shared local validator, database,
backup, historical `validation.json`, and historical graph planning evidence
were not edited during implementation. Canonical deployment remains the
coordinating agent's final step; ZE-104 stays open until that succeeds.

## Reproduced failure and repair

The historical command `python3 tracker/planning/validate-backlog.py` exits
1 against current data. It now fails at line 37, before the known stale
whole-tracker count assertion: it requires every implementation ticket to
remain `todo` and blocked. The named regression
`test_current_backlog_accepts_normal_progress` likewise failed against an
exact copy of that source. Raw output is in `original-red.log.gz` and
`named-current-red.log.gz` in the adjacent `ZE-104-planning-validator/`
directory. These are actual failures, not predicted results.

The replacement retains the fixed original ZE-32 through ZE-78 inventory,
eight named workstreams and draft mappings, exact candidate/index/hash,
ticket scope, original dependency and release-closure checks. It explicitly
reconciles the accepted later amendments, without blanket subset matching:

- ZE-101's exact two scope-text replacements for ZE-62 and its ZE-51 gate.
- ZE-107 as a ZE-71 prerequisite; ZE-106/107 as ZE-78 prerequisites.
- ZE-113 through ZE-116 as ZE-32's qualification prerequisites.
- The exact process-lock and packaging follow-up prerequisites; ZE-108
  remains deferred after first release and outside the release closure.

Tracker-wide totals and frontier are no longer acceptance constants. The
report observes current totals and the original implementation inventory's
status/frontier. `in_progress` or `done` implementation still requires the
review and every actual prerequisite to be done. Before review acceptance,
all implementation must remain todo. Unknown dependency endpoints and
cycles fail, as do missing or unexpected prerequisites and altered scope.

The validator still checks SQLite integrity and foreign keys read-only,
HTTP availability, local Markdown links/anchors, descriptive ticket labels,
trailing whitespace, export prefix, and `git diff --check`. It requires all
ten original plan Markdown files and also checks every additional
`docs/graph/plans/*.md` file. The current report goes to stdout, so running
validation does not overwrite the historical report or backup. This is
documentary validation, not graph runtime acceptance.

## Controls and final worktree results

```sh
python3 -m unittest discover -s scripts/planning -p 'test_*.py' -v
python3 tasks/evidence/ZE-104-planning-validator/test_current_regression.py
python3 scripts/planning/validate-backlog.py
git diff --check
```

All pass: **15 synthetic contract tests**, the named live-data regression,
and full validation against the shared tracker and copied local documents.
The fixture contains only synthetic records; no private tracker export is
checked in. Negative controls cover required tickets and epics, changed
keys/scope/type/labels/hashes, missing or extra dependencies, missing
qualification gates, cycles, unresolved endpoints, incomplete review or
prerequisites for both active/done work, exact ZE-101 text, release closure,
follow-ups, and malformed Markdown. Unrelated epics and tickets, normal
status progress, and valid added plan documents are positive controls.

Four isolated validator mutations each made a targeted test fail with
`ValueError not raised`: weaken exact prerequisite equality to subset
acceptance, skip active/done prerequisite enforcement, skip ZE-62 scope
comparison, or skip candidate hash comparison. Each was restored byte for
byte, then all tests were rerun. `mutation-controls.json` records exact
substitutions, test names and final restored source SHA-256; raw RED logs
and `unit-green.log.gz` preserve the actual output.

Independent review found that an intermediate implementation checked only
the ten required plan filenames, unlike the original glob. The new named
test `test_extra_plan_documents_are_validated_without_losing_required_files`
was observed RED on a broken `plans/extra.md` link before the fix. It now
passes, proves that a valid extra document raises the checked count to 14,
and still rejects removal of a required original file. Raw RED is
`extra-plan-red.log.gz`. The final suite includes this correction.

Observed current documentary results (`current-green.json`):

| Check | Result |
| --- | ---: |
| Current epics / tickets, informational | 11 / 116 |
| Required workstreams / implementation tickets | 8 / 47 |
| Candidate prerequisite edges | 122 |
| Original / amended implementation edges with review | 169 / 177 |
| Original implementation release closure | 47 |
| Original implementation todo / active / done | 43 / 2 / 2 |
| Cycles / unresolved dependencies | 0 / 0 |
| Required release follow-ups | ZE-106, ZE-107 |
| Deferred after release | ZE-108 |
| SQLite integrity / foreign-key violations | ok / 0 |
| HTTP status / Markdown files checked | 200 / 13 |
| Broken local links or anchors | 0 |

These counts describe this invocation, not fixed expectations about future
tracker growth. Final main-checkout counts may change as other work proceeds.

## Reviewed local deployment

The original title-label assertion exposed three subsequent documentary
regressions: key-only ZE-111/ZE-112 labels in the plan index and Cypher plan.
The coordinating agent authorized replacing exactly those labels with the
live ticket titles. URLs and all other document bytes remain unchanged.
`title-labels.patch.gz` preserves the complete bounded patch; the ignored document
tree is not part of this commit. Required ignored link targets were copied
byte-identically into the worktree for validation, not rewritten.

After cherry-picking this commit, the coordinating agent deploys:

```sh
gzip -dc tasks/evidence/ZE-104-planning-validator/title-labels.patch.gz | git apply --check
gzip -dc tasks/evidence/ZE-104-planning-validator/title-labels.patch.gz | git apply
cp tasks/evidence/ZE-104-planning-validator/legacy-wrapper.py \
  tracker/planning/validate-backlog.py
python3 tracker/planning/validate-backlog.py \
  > /tmp/ze-104-canonical-validation.json
```

First check the wrapper/document deployment preimage hashes in
`deployment-preimages.json`. Its backup hash is point-in-time evidence,
not a gate against unrelated agents legitimately exporting newer state.
The wrapper is a small local compatibility entrypoint to the tracked source.
Its path resolution and argument forwarding were rehearsed in a separate
temporary tree with `--root` selecting the worktree: full validation passed
(`wrapper-rehearsal-green.json`). The source also accepts `--root` for
read-only checks against another checkout. No historical recorded result
is overwritten or reinterpreted as present-day product acceptance.

The root agent will capture the current backup hash immediately before and
after validation to prove the command does not overwrite it. Historical
`validation.json` and `graph-detailed-planning.md` remain pinned. The root
agent will record the canonical command result before closing ZE-104. This evidence
does not claim that final deployment has already happened.

All `.log.gz` files preserve raw output bytes; inspect with `gzip -dc`.
