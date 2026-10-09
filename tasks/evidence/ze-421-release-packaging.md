# ZE-421 unified Swift release packaging

Owner requested release 0.7.0 and explicitly authorized takeover here.
Relationship steps S0-S5 are closed and pushed at da2df110. This ticket repairs
packaging independently while the separately owned ZE-420 finishes. Public
publication remains ZE-369 work after its dependencies close.

## Contract and changes

The root ZeppelinEmbed product selects the universal graph-free artifact by
default and the macOS 14+ ARM graph artifact with ZE_ENABLE_GRAPH=1. The graph
module contains its graph C headers; only source-shim builds need the unsafe
C compiler flag. Local XCFramework selection uses the corresponding graph
path. No new product, Intel graph Swift slice, engine dependency, persisted
format or ABI change was introduced.

The release workflow no longer refuses its graph matrix leg. Optional source_ref
supports checksum rehearsal before tagging and prevents asset attachment for
that mode. Both matrix artifacts are retained before checksum verification;
checksum mismatch still fails the job and blocks installed qualification and
publication. The default tag publication path still requires exact checksum
verification and installed consumers before attaching assets.

Installed Swift qualification uses the actual root manifest, complete Swift
sources, and the supplied archive. Remote qualification uses the root product
and exact released Git dependency, with checks of downloaded archive bytes.
The structured C export check now recognizes ze_store_graph_query. The legacy
export checker retains ze_graph_response_free from the frozen allowlist; its
old prefix filter incorrectly removed this existing ABI symbol.

The ignored local scripts/release-local.sh version check was repaired in both
main and the release checkout to read both root checksum markers. It remains
local and ignored. The three protected main user files were not edited.

## Named RED to GREEN

Command: SDKROOT=/Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk python3.13 scripts/release/check-installed-graph.py --self-test

- test_unified_graph_manifest_selects_remote_graph_artifact: RED hit the
  graph/local-FFI precondition; GREEN selected the graph URL/checksum with no
  unsafe settings in the distributable Swift target.
- test_release_workflow_accepts_unified_graph_manifest: RED hit the explicit
  ZE-362 publication refusal; GREEN accepted the unified root graph URL.
- test_profile_contract_recognizes_unified_structured_export: RED claimed the
  public structured query export was absent; GREEN recognized its unified name.
- test_graph_free_exports_retain_the_response_free_abi: RED omitted the frozen
  response deallocator; GREEN retained it. A real legacy archive build also
  reproduced the exact one-symbol allowlist difference before the fix.
- test_remote_qualification_rejects_non_release_tags: RED hit the blanket
  refusal; GREEN rejects invalid tags before network activity. Real remote
  execution remains pending publication, not proved by this regression.

The latest complete self-test run: 11 passed in 0.309s. Initial attempts with
Python 3.9 (no tomllib) and the newer CLT SDK (Swift compiler mismatch) are
separate environment failures and are not intended RED evidence. Xcode's
matching SDK and Python 3.13 were used for the meaningful runs.

## Local artifact and installed execution evidence

Build source production SHA: da2df110b2b99e98fee303bb272a9d574daeac54;
packaging changes were uncommitted during these local builds. Hardware:
Darwin Anups-MacBook-Pro.local 27.0.0 Darwin Kernel Version 27.0.0: Tue Jul 14 21:42:16 PDT 2026; root:xnu-13432.0.94.501.4~1/RELEASE_ARM64_T6031 arm64

Commands:
- scripts/xcframework/build.sh --artifact legacy
- scripts/xcframework/build.sh --artifact graph-cypher
- run_swift_consumer with the exact generated graph XCFramework and root
  manifest (normal Swift sandbox, no source engine rebuild).

Legacy: ARM 4732 KiB, Intel 5479 KiB, both <=5632 KiB. Local zip SHA-256:
df6ca56d13464348409e311533144bd460a976277861b84f9e5f0c00819ea24d.
Graph: ARM 8787 KiB <=12288 KiB. Local zip SHA-256:
b1ed77c46cacb7a1bf0ec965697f7d26c7f93088757b561e74fea2a4f252371b.
The checksum placeholders remain pending the CI-produced archive pins; these
local values do not claim reproducibility with macos-14 runners (BL-167).

Installed Swift compilation completed in 5.59s. The execution receipt confirms
batch, structured query, getters, Cypher and resource APIs, with reopen and
retained null/list/bag results. Removing the product declaration failed with
"product ZeppelinEmbed ... not found", then restoring it passed. Consumer
linked sections: 9545 KiB (reported, no consumer gate).

Existing Apple strip diagnostics were observed and retained: four already-
stripped Intel std archive members and signature invalidation on a measurement
copy of the Swift consumer. Rust compilation produced no warnings. These
successful packaging executions are not claimed as warning-free gate logs.

Raw output, negative controls, and receipts:
/Users/aghatage/Documents/code/zeppelin-embed-worktrees/release-070/.ctx/.
Archive slice/size evidence: tasks/evidence/ze-107-{legacy,graph-cypher}-*.md.
CI checksum pins, SDK consumers, final source revision, and publication proof
will be appended when verified.

Graph C SDK command: scripts/macos-sdk/build.sh --artifact graph-cypher.
Build finished in 1m42s; export checks passed, ARM only, minos 14.0.
SDK SHA-256: b614c95ae603a3f92a4eb2581bd5e15ef432e5a7085e87c0297c5ce30a7801ca.
Latest self-test after workflow rehearsal support: 11 passed in 0.310s.

## Installed substitution control correction

The full installed qualification reproduced RED at its existing legacy
substitution control: it accepted the legacy archive's exact graph export set
and raised "legacy substitution was accepted as graph". The shared export set
is intentional: graph-free builds provide unsupported-build stubs. Replace the
obsolete export-difference assumption with a compiled C call to the frozen
create-with-relationship-types entry point. GREEN requires the typed
ZE_ERR_GRAPH_UNSUPPORTED_BUILD response and an unchanged zero handle.

Complete installed qualification command:
SDKROOT=/Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk python3.13 scripts/release/check-installed-graph.py --artifact-root target --output .ctx/installed-all-fixed

Exit 0. C static, C dylib, Rust and root Swift consumer receipts each confirm
batch/structured/get/Cypher/resources. The installed profile validates 21 cases
for each C flavor. Packaged headers, architectures, deployment target, archive
budgets, missing-input controls and the legacy typed refusal pass. Python and
Node packages were not supplied to this local check and remain unverified here.
Checksum enforcement was intentionally not requested for these local bytes;
zero pins still block actual release verification. Apple strip diagnostics
remain recorded as above; this is not a warning-free packaging claim.

First CI rehearsal run 37929281046 failed at checkout in both legs: Actions
checkout treated the abbreviated 43c7e47a as a branch name. No archive build or
checksum gate ran. The failed-step log was read completely. The next dispatch
uses the full 40-character source SHA.

## Swift Store-document fixture alignment

The public graph fixture must associate each searchable node with a Store
DocumentID through the existing V2 batch seam. Graph-only nodes are not Store
search documents. No engine behavior, ABI or persisted format changed.

Named RED: GraphQueryOptionsTests.testQueryTowerCompatibilityAndExplicitAlignment
returned zero rows (four failed assertions) with its old graph-only node.
Adding explicit DocumentID(0,10) gives GREEN: one test passed in 0.129s.
Named RED: GraphStructuredQueryTests.testStructuredSearchReportsSurviveProjectionAndEligibility
returned zero rows with its two graph-only nodes. Explicit DocumentIDs(0,1)
and (0,2) restore document search. Its explicit Auto weighted-hybrid case then
correctly reports EstimatedVectorScore: the existing fusion contract refuses
estimated vector scores. The fixture now asserts that typed endpoint refusal,
including notCommitted disposition, and adds successful Scan with original
float rescoring. Default hybrid remains a positive case. All projection,
eligibility, identity and report assertions remain. GREEN: one test passed in
0.055s. Intermediate default/Auto and rescore-on-Auto diagnostic failures are
retained separately; they are not successful gates.

The old optional-generation interpolation warning is fixed with explicit
String(describing:), preserving the diagnostic value. The Swift README now
states that searchable graph-created documents require an explicit ID.

Full packaged graph Swift command:
SDKROOT=/Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk ZE_ENABLE_GRAPH=1 ZE_USE_LOCAL_XCFRAMEWORK=1 swift test --jobs 3 --scratch-path target/swift-packaging-graph
Exit 0: 73 executed, eight existing conditional skips, 65 passed, zero
failures in 11.480s. No warnings. Existing skips: two optional bindings-parity
corpora, one private fault bridge, four optional profile-manifest cases, and
the private ABI panic probe. Shipping archives exclude those private hooks;
no new test waiver was introduced.

Full packaged default Swift command:
SDKROOT=/Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk ZE_USE_LOCAL_XCFRAMEWORK=1 swift test --jobs 3 --scratch-path target/swift-packaging-legacy
Exit 0: 31 executed, one existing ABI-panic-probe skip, 30 passed, zero
failures in 10.119s. No warnings. Complete primary logs were read, with no
truncation: .ctx/swift-packaging-graph-GREEN.log (202 lines) and
.ctx/swift-packaging-legacy.log. Named RED/GREEN logs and intermediate failed
attempts are retained in .ctx/swift-query-options-* and
.ctx/swift-search-reports-*. Original full failures are not gate credit.

## First exact CI pins and downloaded-consumer verification

Rehearsal 37929723958, source 0ec28f50b42bf8ac16f5335d48d6d666189f4ecf,
macos-14-arm64 image 20260831.0302.1, macOS 14.8.9/23J631, stable Rust
1.93.0, nightly-2026-07-01 1.98.0-nightly/f46ec5218, Xcode 15.4.
Both artifact builds passed their architecture/export/size checks and uploaded
archives before intentionally failing verification against zero placeholders.
Qualify/attach were skipped; nothing was publicly released. Complete Actions
log (1424 lines) was read through its metadata-normalized form (1451 lines),
including existing strip and Actions Node deprecation diagnostics. The run is
not claimed green. Downloaded file SHA-256 values exactly match producers:

- legacy XCFramework: 4016bfdeaecae0617ef3f3e879b80451601b53902194fc384ec6eb67a58705b9
- graph XCFramework: 18fe397689d18f6f0225b8faf6d0456ee0c47f211a95c101096b0a4934dbda15
- legacy C SDK: bb31e0aabbe9e8bd9c1c02ea06e4899ba0aef508b558576ef70c92e868626053
- graph C SDK: 278e46f755d8f5d47a159fa6b173bfd8cee46280d5255e24830ae381d477c8a7

Root manifest now pins the two XCFramework hashes. Checksum-enforced command:
SDKROOT=/Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk python3.13 scripts/release/check-installed-graph.py --artifact-root .ctx/ci-rehearsal-37929723958 --enforce-checksum --output .ctx/installed-ci-pins
Exit 0. Actual/pinned graph hash matches. C static, C dylib, Rust and Swift
receipts all pass; 21 profile cases for each C flavor pass. The legacy
substitution returns the required typed unsupported-build refusal. Complete
primary log read: .ctx/installed-ci-pins.log. Existing strip diagnostics remain
recorded, not claimed warning-free. Consumer Swift build: 5.57s.
Exact CI archive sections: legacy ARM 4732 KiB, Intel 5478 KiB <=5632;
graph ARM 8786 KiB <=12288. Graph core 7911 KiB reported separately with no
second gate. Latest 11 checker self-tests pass in 0.886s, full log read.

These pins cover production da2df110. ZE-420 is still separately owned and
active; any later production change requires fresh final-source archive proof
and matching pins before ZE-369 publishes. No ABI/format/golden changes.

## Packaging branch contract gates

At 1d3dfabf, production Rust matches da2df110. All primary output lines read:
- cargo fmt --all -- --check: exit 0, no output.
- cargo clippy --workspace --all-targets --features zeppelin-embed-workspace-tests/graph-result-test-support -- -D warnings: exit 0, 43.58s.
- cargo clippy -p zeppelin-embed --features graph-cypher --all-targets -- -D warnings: exit 0, 22.34s.
- RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --features zeppelin-embed-workspace-tests/graph-result-test-support: exit 0, 46.18s.
- xcrun clang-format --dry-run --Werror bindings/node/native/addon.cc: exit 0, no output.
- cargo test -p zeppelin-embed --features graph-cypher --test format_compat: exit 0, three passed/four existing configured ignores, 0.22s.
- cargo test -p zeppelin-embed-ffi --features graph-cypher --test ffi_contract: exit 0, 21 passed/zero ignored, 7.18s.
No warnings/errors. Logs: .ctx/ZE421-static-*.log and
.ctx/ZE421-compat-*.log. Shared task-owned rel-speed target cache was used;
no target directory owned by another session was modified.
Pinned workflow rehearsal 37931652517 is still running; no green workflow
or public-release claim yet.

Read-only credential preflight: npm whoami authenticated as zep-dev (exit 0)
using the ignored owner's NPM_TOKEN through an ephemeral mode-0600 npmrc.
No token was printed or committed. Permission-listing requests returned 403
from /-/org/zep-dev/package, so package write scope is not yet verified; this
is not an expired-token conclusion. Actual npm publication remains required
under ZE-369. No registry version has been published by this session.

The attempted crates.io /api/v1/me read returned HTTP 403 with the explicit
reason "this action can only be performed on the crates.io website". It does
not validate or invalidate CARGO_REGISTRY_TOKEN. Actual cargo publication is
still required; no token value was printed or committed.

## Pinned rehearsal GREEN on macOS 14

Swift Release run 37931652517 succeeded at source
1d3dfabf39bd70fba22042269e0ed16317f631b6. Both archive build/checksum jobs
and exact uploaded-distribution qualification succeeded. Attach-release was
SKIPPED by source_ref rehearsal mode; no tag/release asset/public registry
publication occurred. Full readable primary Actions log (2162 lines) read
without truncation: .ctx/ci-pinned-readable.log; original retained alongside.
Existing Intel strip/signature-copy diagnostics and Actions Node deprecations
remain present. A competing cache-save reservation failed in post-job cleanup;
it did not affect archive bytes or job success. This is not a warning-free
Actions-log claim. Local warning-denied gates above remain clean.

Both rebuilt zip hashes exactly reproduce the pinned discovery values. New
SDK tarballs, which are not Swift checksum pins:
- legacy: 8178f922d4f7750729e8b6a2672f3bd776f58df09f96980525121f3034a85316
- graph: a6f08996c289b99181ad9b4989b63521e28b67a593e66e7716681e23fa6d030b
SDK tar hashes are recorded per exact run; byte identity across runs was not
assumed. Installed qualification ran on macOS 14.8.9/23J631 ARM VM, with
Xcode 15.4 and Rust 1.93.0. Downloaded report:
.ctx/ci-pinned-installed-evidence/report.json. Actual graph checksum matches;
C static/dylib, Rust and root Swift receipts execute batch, structured query,
get, Cypher and resources successfully. Each C profile flavor has 21 passing
cases. Swift consumer build completed in 7.34s. Legacy substitution returned
ZE_ERR_GRAPH_UNSUPPORTED_BUILD. Sizes remain 4732/5478 KiB legacy and 8786 KiB
graph, under their unchanged budgets. Python/Node remain outside this packaging
qualification and are required separately by ZE-369.

ZE-421 packaging acceptance is verified. Final publication remains ZE-369,
which waits for ZE-420 and final-source archive/full-suite/adversarial checks.
