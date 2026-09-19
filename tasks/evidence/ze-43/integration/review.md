# ZE-43 main integration

Integrated reviewed source `190e702e996f1638993566e88ccfd10e885089f3`
(parent `81e6e95`) onto main `af4a1f1`. This is the complete component
candidate; no qualification boundary is widened by the cherry-pick.

All 61 source/seed hashes and 267 evidence-manifest entries were verified
against committed bytes. All 329 candidate paths match the owned allowlist.
The integrated tree retains 322 files exactly and combines seven shared
files using complete committed source plus exact independent additions.
The six merge conflicts were not resolved with conflict-marker unions.

- Artifact framing retains the entire candidate and main's append-only
  adjacency tags 13/14 plus their decoder arms. Native tags 11/12 remain.
- Storage module registration retains adjacency and all new native modules.
- Fuzz targets and adversarial tests append the complete candidate additions.
- Coverage, module and runner registration insert the exact PG8 additions
  into the full main files, preserving every previously landed probe.

Patches, full per-path hashes and exact merge recipes are retained here.
Main verification passes 58 affected storage tests, 12 adjacency tests and
both actual PG8 tests with graph-cypher enabled. The latter runs the combined
canonical runner, including the landed graph/C owner probe. Strict scoped
core/test and runner Clippy, changed-source formatting and source diff checks
pass. Nextest uses four isolated processes with zero retries.

Earlier independent COW, ownership, real PG8 and admitted-reference reuse
reviews are retained in ../reviews. The final two test-only corrections
are included in this main run: a previously valid reference in the same
failed pack is refused, and PG8 proves the exact current private artifact
backs each KeyFences reuse receipt. No source mutant remains.

All 45 current inherited file hashes match the previous integration;
unrelated dirty files were neither staged nor changed. Their earlier
historical baseline had two external instruction edits, which this does
not reclassify as root changes. All worktrees remain intact.

Broad workspace/adversarial/coverage/size/workload qualification remains
ZE-118. Actual OUT/IN integration, admitted views, durable publication,
recovery and reclamation remain ZE-44/45/39/40/46. The component's physical
artifact reopen and model fixtures are not GraphStore lifecycle acceptance.
