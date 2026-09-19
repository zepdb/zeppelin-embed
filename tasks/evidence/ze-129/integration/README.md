# ZE-129 main integration

Source candidate `732cb3d3716e0b74b498b9eec9bcd01d7d341adc` is integrated onto main base `aabdc39fa3819aa9bd4b4afba0d356b1bd14704e`. All seven candidate paths remain: six byte-exact and the module export combined additively with complete main. Four manifest hashes, including the unchanged legacy codec oracle, were checked against the committed candidate. The initial conservative merge assertions stopped before edits; Git merged the module insertion cleanly, and exact additive preservation was subsequently verified.

Main focused nextest passes all eight adjacency/liveness oracle tests with process isolation, four test processes, retries zero, and one libtest thread per process. Strict all-targets oracle clippy and formatting pass. The std-only production model and tests are unchanged from the independently reviewed candidate. No dependency or lockfile changed.

The independent Sol/xhigh review found no Standards or Spec blocker and its directed primitive probe passed. Full report and probe are retained. Permanent-ID reuse and duplicate-target rejection are present in the model but lack dedicated named candidate tests; the independent probe exercised nonreuse. Actual native producer/observer and seeded runner acceptance remain ZE-44. This model acceptance does not prove physical storage, lease admission, publication, recovery or public traversal.

`checks.json` holds exact main commands and results; `correspondence.json` and `preservation.json` pin source and all 45 current inherited files. `raw-integration.tar.gz` retains exact logs, review, probe, merge notes and records. Broad workspace/adversarial/coverage qualification remains deferred ZE-118; no such suite is claimed passed. Existing workers, worktrees and inherited dirt are preserved.
