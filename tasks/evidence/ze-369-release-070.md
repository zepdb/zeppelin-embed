# ZE-369: release 0.7.0

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
Node publication workflow. Its v0.7.0 dispatch carries the single explicit
ZE316 waiver; this session does not rerun a broad local suite.

## Publication and installed verification

Pending actual v0.7.0 artifact publication workflows and public installed
checks. The release notes are extracted from the 0.7.0 CHANGELOG section.
No publication success is claimed at the pin-update commit.
