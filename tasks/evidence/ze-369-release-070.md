# ZE-369: release 0.7.0

Release complete: GitHub v0.7.0 is public/latest/stable; npm, PyPI,
crates.io, Swift/C SDK assets and Homebrew 0.7.0 are published. Minimal
packaging and public installed checks passed. Source/tag is immutable at
b9b5f7be93248a4751bd1c4bd301f85528329513. No adversarial campaign or
repeat broad local suite was run under the owner override.

## Authorized verification scope

2026-10-09: the owner explicitly requested no adversarial campaigns,
minimal required testing, and publication of 0.7.0. This supersedes the
older one-hour campaign and repeat-long-local-full-suite prerequisites.
ZE-423 was closed Out of scope; no executed baseline/campaign credit.
ZE-420 engine fix is landed/pushed at
927e7f7c0aef26a7236d9f08e050d300f17e0c3a; remaining separately owned
checks are not credited here. Main CI remains waived. Artifact release
workflows and installed smoke checks remain required.

## Final archive pins

Swift checksum discovery run 37936744110 built source
927e7f7c0aef26a7236d9f08e050d300f17e0c3a on macOS 14.8.9 arm64,
runner image 20260831.0302.1, Xcode 15.4, stable Rust 1.93.0 and
nightly-2026-07-01 (actual 1.98.0-nightly f46ec5218).

Exact downloaded XCFramework SHA-256:

- Legacy: `4016bfdeaecae0617ef3f3e879b80451601b53902194fc384ec6eb67a58705b9`
- Graph: `b33dae4c31e46043c338faa1ae913132ae9a951bdf052f8f211d680ebd73f9e7`

The legacy archive retained its pin. The graph archive changed with ZE-420
and its pin is refreshed in Package.swift. This discovery run is FAILED,
not release qualification: its only build-job failure was the expected
old graph-pin mismatch after packaging/upload; installed qualification
and attachment were skipped. No assets were published by that rehearsal.
All 1,402 raw terminal log lines were read after retaining the logs in
`.ctx/ZE369-final-discovery.log` (prefix-stripped readable copy retains
every line). An initially truncated chunk was reread in smaller chunks.
Four existing Intel strip warnings and Actions Node deprecation warnings
remain visible; this is not a warning-free CI claim.

Linked sections: legacy arm64 4,732 KiB, legacy Intel 5,478 KiB, each
within 5,632 KiB; graph arm64 8,783 KiB within 12,288 KiB. Graph-contained
core 8,096,809 bytes / 7,908 KiB is reported without a separate gate.
SDK discovery tar SHA-256 (tarballs are not assumed reproducible):
legacy `7d4ae11de691b9d1f648a979fd56cac8314698ee285c8b0ec515749748621112`,
graph `5c054878de9e7f81895f4ce0f543d0d41853c4d856181e6ac03046d75b887854`.

## Focused prepublication checks

Host Mac15,9 / M3 Max / 128 GiB / macOS 27.0 (26A5388g); Node 24.21.0,
stable Rust 1.93.0, Python 3.13.7. Commands from release-070 worktree.

- `CARGO_TARGET_DIR=../rel-speed/target SDKROOT=/Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk npm run build:native`
  from bindings/node: rebuilt arm64 and Intel shipping binaries at final
  engine source. Release builds 1m46s and 1m41s, exit 0; full output read.
- `node --test bindings/node/test/store-graph.test.mjs bindings/node/test/graph-list-params.test.mjs`:
  eight passed, zero failed/skipped/cancelled, 406.382167 ms; output read.
  Covers one-Store documents/vectors/relationships, explicit graph enable,
  async copied inputs/cancellation, unchanged graph-free bytes on invalid
  calls, typed unsupported builds, nested lists and list bounds.
- `npm ci --ignore-scripts --no-audit --no-fund`: 23 existing locked dev
  packages installed, 423 ms. `npm run typecheck`: exit 0, output read.
  Initial typecheck lacked local tsc (127); it is not passing evidence.
- `check_release_contract(archive=<exact downloaded graph zip>, enforce_checksum=True)`:
  RED discovery rejects old pin; GREEN updated pin equals exact archive
  SHA. This validates the pin contract, not installed reachability.
- `python3.13 scripts/release/check-installed-graph.py --self-test`:
  11 passed, 0.933 s; output read. `git diff --check` passes.
- Registry credentials synchronized to the existing crates-io and npm
  environments without logging secret values. Environment metadata has
  no manual approval protection rules.

ZE-421 and ZE-422 retain earlier actual installed-distribution and
release-tooling proof. The Windows archive budget is enforced during the
Node publication workflow. The initial single-test waiver was superseded
by the owner minimal-testing override described below.

## Publication and installed verification

Annotated v0.7.0 is immutable at
b9b5f7be93248a4751bd1c4bd301f85528329513. Release notes use the exact
0.7.0 CHANGELOG section. Rust workflow 37938350652 and Python workflow
37938354770 succeeded against this tag. Rust registry checksum:
`1dba32f68dbf9dbaef4116d645d58039bfc2fbac4687a5d8eef5a6913da698e0`.
PyPI macOS arm64 wheel checksum:
`a5e026e9ef438551aa370b919d4bd9de7c8a6abc73ae1e1386bf396bd2dcc3bb`.

Public registry-installed smoke commands (outside any source checkout):

- `cargo +1.93.0 run --manifest-path /private/tmp/zeppelin-070-installed-v03rqxi7/rust/Cargo.toml`:
  exact registry dependency `zeppelin-embed = "=0.7.0"`, downloaded then
  built with a fresh external target; Store open/close succeeds, exit 0.
- `python3.13 -m venv /private/tmp/zeppelin-070-installed-v03rqxi7/python`,
  then that venv's pip `install --only-binary=:all: zeppelin-embed==0.7.0`:
  public wheel installed; copied installed_smoke.py run with `python -I`
  succeeds, exit 0; installed metadata confirms 0.7.0.

Logs retained in release-070/.ctx/ZE369-{rust,python}-installed.log.
Actual installed output read; full publication logs retained separately.
No broad-suite pass is inferred from these smoke checks.

## Publication tooling repair

Initial Node publication 37938346664 failed before package tests:
Windows whole-archive llvm-strip rejected short COFF import objects;
macOS ffi_graph_header::ze399_external_c_applies_queries_and_reopens_a_document_node
built an ARM dylib for an Intel C harness (11 other header tests passed).
Windows full log includes 612,518 lines of mostly dumpbin headers;
only relevant failing-step diagnostics were inspected, not a claim that
all 612,518 lines were read.

Tooling-only commit 25be3c728074ada081e9a4cc921758dab1ce7c17 repairs
measurement through the existing COFF member parser. Ordinary objects
are stripped individually; short imports and original archive bytes are
preserved. The 12,288 KiB cap and original-archive hash receipt remain.
A separate checkout of the workflow's tooling revision supplies this
measurement helper while packaged source stays at immutable v0.7.0.

- RED: whole-archive llvm-strip rejects the real COFF fixture; named
  `CoffMeasurement.test_measurement_preserves_imports_and_strips_debug`
  fails before the new measurement mode exists.
- GREEN: same test passes (0.540 s), confirms untouched original bytes,
  preserved imported symbol and body, native symbol, absent debug
  sections, and usable llvm-size output.
- `ReleaseTests.test_windows_shipping_archive_rejects_over_budget` passes
  (0.267 s): 12,288 KiB + 1 byte is rejected by the wired graph gate.
  This oversize fixture is Mach-O on the local Mac; actual Windows size
  is verified by the publication workflow, not inferred from it.
- Bash syntax, workflow YAML parsing and git diff whitespace checks pass.

Only v0.7.0 dispatches defer broad Rust and Node crash campaigns per the
owner's explicit request. Graph smoke, types, architecture/exports,
archive caps, installed-package and Electron checks remain. Normal PR,
push and later-version runs retain their full suites. Main CI automatic
push runs are waived and cancelled; no adversarial campaign is run.
Node retry: 37939927801 from main tooling, source input v0.7.0.
Swift/C SDK publication 37938342777: SUCCESS (all four jobs). Actual
uploaded-distribution C static/dylib, Rust and Swift consumers executed
batch/structured/get/Cypher and resource checks, exit 0. Exact uploaded
archive budgets: legacy ARM 4,732 KiB / Intel 5,478 KiB under 5,632;
graph ARM 8,783 KiB under 12,288. Downloaded installed evidence includes
raw platform measurements and report.json. Actual publication SDK tar
hashes differ from discovery tarballs, as expected (not reproducible pins):
legacy `c7e92f10a2fd48c6b3cd72a420ffdda5ccbadf326c137ab6fa5db2290c932861`,
graph `3f91a446ab0e6777527df71efd99f67b2b89383339aede73452807e1a44fab7e`.

Node retry 37939927801: Windows archive/exports/packaging PASS. Actual
Windows linked sections 12,144,245 bytes / 11,860 KiB under 12,288;
physical archive 25,539,390 bytes. Seven focused graph tests passed;
one source-only unsupported-build fixture failed module resolution on
`/D:/...` before binding execution, due to using file URL.pathname.
The remaining Mac build was cancelled after this failure. No complete
publication/runtime pass is claimed for this run.

Tooling-only bd7d4bbbb6eec9337221bd73008e355a9e219980 removes the redundant
Windows source smoke step for the minimal release. The existing installed
package smoke remains mandatory and exercises real graph apply, query
and reopen. Full normal suites remain unchanged; source/tag stays b9.
Latest Node retry 37941103445 uses this workflow and v0.7.0 package source.

GitHub 0.7.0 is PUBLIC, stable, latest, with all four Swift/C assets:
https://github.com/zepdb/zeppelin-embed/releases/tag/v0.7.0

`SDKROOT=/Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk python3.13 scripts/release/check-installed-graph.py --remote-tag v0.7.0 --output .ctx/ZE369-remote-installed`
PASS on local Mac: fresh external clone resolves exact tag b9; exact
public graph zip checksum matches b33dae4c...; public graph SDK loaded
by C static/dylib and Rust consumers; independent SwiftPM consumer uses
remote `.package(url: ..., exact: "0.7.0")`, public binary download and
ZeppelinEmbed product. All four executed batch/structured/get/Cypher and
resource checks, exit 0. Complete terminal output read, report retained.
Annotated-tag clone warning and strip warnings for measurement copies
are retained; no warning-free claim. No engine rebuilt for this proof.

Homebrew workflow 37941157199: SUCCESS, update and hosted macOS 14
install/test jobs. Formula commit
2a5cd0b1de8f2e3e00334aa92d36d14f7b3e6061 sets version/URL 0.7.0 and exact
public legacy SDK SHA c7e92f10a2fd48c6b3cd72a420ffdda5ccbadf326c137ab6fa5db2290c932861.
`ruby -c`, `brew style`, `brew trust`, `brew install zepdb/tap/zeppelin-embed`
and `brew test zepdb/tap/zeppelin-embed` succeeded. Actual installed C ABI
consumer compiled and ran `./abi`, exit 0. This is an installation/ABI
smoke, not a broad graph-free suite. Raw log retained in
.ctx/ZE369-homebrew-publication.log; relevant test output inspected.
npm publication results remain pending below.


Final Windows job 113855395062 in Node run 37941103445: SUCCESS on
Microsoft Windows Server 2025 / windows-2025-vs2026, Rust 1.93.0,
Node 24.21.0. Exact tagged package source with separate bd7 tooling.
Real shipping archive remains 12,144,245 linked-section bytes / 11,860
KiB, cap 12,288 KiB; physical 25,539,390 bytes. Archive native-symbol
identity, absent LLVM bitcode, addon export/strip checks passed.
`npm run typecheck`, `npm run check:prebuilt`, `npm run smoke:install`
passed. Installed runtime says graph open/apply/cypher/reopen win32-x64
ok; installed TypeScript smoke passed. Shipping Windows Node and Electron
binaries uploaded as prebuild-win32-x64. Relevant actual size and installed
output read; full raw log retained .ctx/ZE369-windows-final.log.
No broad Windows Rust or crash-suite pass claimed.


Final macOS package job 113855394897 in Node run 37941103445: SUCCESS.
Both shipping graph FFI archives pass: ARM 9,103,079 linked bytes / 8,890
KiB (physical 19,176,824 bytes); Intel 10,930,076 bytes / 10,674 KiB
(physical 20,463,848 bytes), each under 12,288 KiB. Archive packaging,
native definitions/exports and architecture checks passed. Focused graph
smokes passed on ARM and under actual x86_64 Node/Rosetta; installed
packed-package ARM graph smoke and installed declaration check passed.
Mac shipping prebuilds uploaded; assembly job 113860438065 SUCCESS.
Full raw log retained .ctx/ZE369-node-macos-final.log; relevant actual
size/test output inspected. No broad Rust or crash-suite pass claimed.


Node workflow 37941103445: all eight jobs SUCCESS, including ARM/Intel
signed Electron lifecycle/isolation, complete advertised-binary tarball
assembly, and publish. npm accepted 0.7.0 at 14:16:35Z and reported it is
being processed and may take a few minutes to become available. Do not
republish. Provenance is signed and recorded at transparency log index
3165869601 (https://search.sigstore.dev/?logIndex=3165869601).
Full publish log (128 lines) read; processing notice, 2FA-token migration
notice and Actions deprecation warning retained; no warning-free claim.
Exact assembled tarball SHA-256:
`a0c4da3006f1dd54adacff9b85177d25b7bc52e34bb98cfbac25a799985ff976`.
Publish-reported SHA-1:
`15487b7b96f2d3a489a5bd7b03c07e1b6b0b666f`.
Four binaries are included (ARM/Intel macOS, Windows Node NAPI8/Electron44),
11 total files, 15.4 MB packed / 37.6 MB unpacked. Initial public registry
reads returned 404 while npm processing was pending; these are not passing
installed checks. Public-registry install proof follows after processing.


## Final public npm installation

Registry processing completed: public metadata's latest tag is 0.7.0.
Public tarball downloaded from
https://registry.npmjs.org/@zepdb/zeppelin-embed/-/zeppelin-embed-0.7.0.tgz
is byte-identical to the exact qualified CI tarball (SHA-256 a0c4da30...);
registry SHA-1 and complete SHA-512 integrity match its bytes:
`sha512-TiFXE2dFzQgWqDMcLMaDpbub2g+GRJzUqqHgCgkROviL6yW8/RdfJa6EQK/RNhOG+BylSDY6AU1Ve14Zl916gg==`.
Metadata includes signed registry signatures and SLSA provenance URL.
No second publish was attempted while processing.

Outside checkout, in /private/tmp/zeppelin-070-installed-v03rqxi7/node:

- `npm install --registry=https://registry.npmjs.org --min-release-age=0 --prefer-online @zepdb/zeppelin-embed@0.7.0`:
  exit 0, one package installed; no global age policy changed.
- `node smoke.cjs` (copied installed-package-smoke.cjs): exit 0, exercises
  vector, lexical, hybrid, cancellation, prefix, verify API/CLI, plus graph
  enable/apply/query/reopen; says installed graph darwin-arm64 ok.
- Existing local TypeScript compiler with `--module node16 --moduleResolution node16 --target es2022 --strict --noEmit type-smoke.ts`:
  exit 0 against installed declarations, no added dependencies.
- Installed metadata confirms 0.7.0. An initial optional metadata probe
  through `require('@zepdb/zeppelin-embed/package.json')` correctly failed
  because that subpath is not exported. Reading the installed manifest
  beside the resolved public entry succeeds; no package changes made.
  The preceding runtime and type commands already succeeded, so they
  were not repeated for this probe correction.

Actual output read, retained in .ctx/ZE369-npm-installed.log and
.ctx/ZE369-npm-metadata-GREEN.log. Registry metadata and public tarball
are retained beside the shipping tarball for byte comparison.

Tracker export `node --no-warnings tracker/cli.mjs export tracker/backup.json`
was refreshed: 15 epics, 423 tickets, 623 dependencies. Tracker files stay
local/gitignored by project contract. ZE-420's separately owned remaining
broad checks are not credited or closed here. ZE-423 remains Out of scope;
there is no campaign/baseline credit. S0-S5's accepted latency misses
remain recorded in their closed resolutions and
`tasks/evidence/ze-perf-unified-relationships.md`.
