# ZE-140 mutation lowering evidence

## Frozen source and authority pins

- Product/test source commit: `bde36aa86906f3e39b59e2e50ae19fc307de0f7b`
- Product/test tree: `d66e513ef9f24d8c3fb1243698353528f7be19d3`
- Binary diff SHA-256 from the exact base: `16123b17f81c322d2061635a7e57ca9bd11013ed7f27499db64d628c1cd2d9cc`
- Base: `d406d34f354ca617e360ca0b46d56d85c38bd76c`
- Branch: `codex/ze-140-mutation-lowering`
- Astra plan: `/tmp/ze-140-astra-mutation-plan.md`, SHA-256 `a34c610bd942f13542074daca16fe3b98ed61582f260e8036a559909c40b2b0b`
- Source manifest: `/tmp/ze-140-astra-source-manifest.json`, SHA-256 `97c809bcfdc59b687bcdc7592e4f99e375b34f606978d9d471120e3c4d8e41da`
- Preservation manifest: `/tmp/ze-140-preservation.json`, 57/57 entries verified, SHA-256 `bcf13d3a4289230ff33eae33328a759084d40c6f15b213ca5a3b2600063af9ed`
- Final diagnostic re-review: `/tmp/ze-140-spec-review-final2.md`, SHA-256 `0540c03f317d1bd5a36c45865e634d66898f981a009ed40db7c6ce24cb71f610`
- Final source hashes: `lowering/mod.rs` `22e3babdbbcc30f4b08901fc4aa608273c22430145d3e55cdc4525cc819e8e62`; `lowering/mutation.rs` `cb68c7faf4b052cb5be1ff2dc126eeffcfbf3e554da8022e5fc0743805557547`; core mutation validator `45d08e1888198bc796635a114809b0e7cfe22027b2eb9718f7536fca7f536889`; PG21 production probe `02b80184025db3ece70d50b71b82939f747d9aa71e6563bba6ad15c6991efcca`; PG21 oracle `e9ae42c4c6299961c4e08ad2b9edc4a45ce951892fa7f8f05913cb642b4dca59`.

The allowed delta is the literal 30-path inventory in `commands.json`: 18 product/test Rust paths and 12 evidence paths. No Cargo manifest, lockfile, runtime executor, staging, storage, writer, FFI/header, public graph API, or TCK file changed. The canonical tracker is the inherited `tracker` symlink and is not part of the Git delta.

## Pre-edit baseline and RED to GREEN

All three required pre-edit baselines ran in isolated detached worktrees at exact base `d406d34f354ca617e360ca0b46d56d85c38bd76c`. `base-core.log` contains the exact command, clean-source metadata, raw output, run `628e8dde-c3fd-4e9d-ad83-e50cb2353e4c`, exit 0, and 26/26 result. `base-compiler.log` contains the exact command, clean-source metadata, raw output, run `67290b1d-b05b-422e-9927-e13b053df429`, exit 0, and 53/53 result. `base-adversarial.log` contains the exact worktree lifecycle, command, raw output, run `327b754a-0763-4db9-a8c5-2f5ec0f29ee3`, exit 0, and 3/3 relevant-probe result. These are narrow changed-path baselines, not a broad campaign.

The first core RED was `detach_delete_accepts_node_relationship_and_null_targets`: relationship-target `DETACH DELETE` returned `PlanError::Type`. The compiler RED was an unresolved `compile_mutation_in` import. PG21 registration then produced the intended additive RED: 191 observed graph keys versus the previous 177-key total. The review correction added a literal PG21 RED, run `6e45ff9c-376d-4129-8162-b8536286b1fa`: seed 0 reported 3 observations and 0 completed orientations against the required 6 and 2. The immediate directed GREEN was run `f7537419-5695-4adc-bb3d-35b25f977fcb`. Final spec review identified the unasked `LoweredPlan` carrier and changed read diagnostics; the correction restored the required existing `LoweredRead` carrier and its diagnostics, then 35/35 affected compiler tests passed in run `e6a7ac05-1001-4982-8c83-b869da6773a5`. The final focused re-review then caught two remaining route-dispatch strings. `read_lowering_preserves_typed_profile_errors` produced RED run `e96713d4-1b25-400f-bbfc-77765c5fe42e` with `"lowering route clause"` versus `"read lowering clause"`; GREEN run `67bf83c4-9dc3-4dfd-9b41-e5f297c3a518` preserves `"read lowering clause"` and the fallback `"unexpected read clause"` while mutation uses mutation-specific text. Exact raw output and exits are in `diagnostic-red-green.log`.

## Exact final command receipts

`final-command-driver.py` is the checked executable runner for all 16 final focused commands. It spells out every argument, including all 18 owned Rust paths for `rustfmt`; no placeholder remains. `final-command-raw.log` contains each literal command, expected exit, unabridged combined stdout/stderr, actual exit, output SHA-256, and receipt assertion. `final-command-receipts.json` contains the corresponding structured records. `commands.json` links those artifacts and preserves their hashes.

The final focused nextest runs are GREEN:

- Core plan: 27/27, run `606c2446-b18d-4376-9f9e-d64d75f88a3a`.
- Compiler: 69/69 across seven binaries, run `55c285f7-d71a-4e5a-a7cf-30d48a3832b7`.
- Graph adversarial matrix: 7/7, run `2aaa8376-f571-455b-854c-83c823b8fb0b`.
- Hook-enabled registry controls: 2/2, run `65cd86a3-4114-4dc1-bb6f-51c3b29beddd`.
- No-graph registry negative control: 1/1, run `039ec089-8381-4789-804b-458997bee8e1`.
- Serial libtest isolation: 22/22 compiler allocation/mutation/runtime tests. Documentation has one explicit compiling example and 5/5 compile-fail tests, including the four mutation no-escape contracts.

All 11 required source mutants produced the intended failure, matched the named assertion, restored to their exact before hash, and passed the exact focused test immediately afterward. `mutant-driver.py` is the checked executable driver. `mutant-receipts.json` contains each exact replacement, test command, exit, complete raw RED/GREEN output, before/mutant/restored hashes, and assertion-fire Boolean; `mutant-raw.log` is the same unabridged terminal receipt. The restored `lowering/mod.rs` hash is `22e3babdbbcc30f4b08901fc4aa608273c22430145d3e55cdc4525cc819e8e62`, and the restored `lowering/mutation.rs` hash is `cb68c7faf4b052cb5be1ff2dc126eeffcfbf3e554da8022e5fc0743805557547`.

## Ownership, allocation, and control receipts

`mutation_lowering_real_allocator_fail_at_each_site_releases_all_backing` passed every real allocator fail site with `mutation_allocator_sites=257`, `real_heap_peak=59224`, `query_reservation_peak=285632`, and final live bytes `56`, equal to the fixture baseline. The plan retains 35 exact allocation owners; deliberate omission of either the mutation arena or mutation-span arena failed the admission/span proof. The returned parameter metadata slice is explicitly verified, and omission of its owner fails `UnprovedInput`. A second `QueryMemory` rejects the charged inventory with exact `UnprovedInput` and no reservation delta. A structurally validated raw-fact plan also fails runtime admission with exact `UnprovedInput`.

The close-first runtime fixture reported `mutation_lowering_polls=3244`, binder completion at poll `1736`, first lowering poll `1737`, consumer entry at poll `3241`, and selected fire `2488`. It finds the exact phase transition by binary probing the real retained-view checkpoint errors, proves both adjacent boundary polls, and selects the fault strictly inside that measured interval. The callback was not entered, close won as `ReadCancelled`, the fire count was exact, and all ownership returned to baseline. The separate final-check fixture discards post-consumer output once and returns all query/shared reservations to baseline.

PG21 passed seeds `0`, `1`, `140`, and `u64::MAX`, each with 6 actual observations covering both incoming and outgoing orientation, 2 fault fires, 2 clean controls, and 9/9 comparator fires. Coverage keys are hit only after both directed observations pass the independent oracle. The compiler-only boundary was 1,468 polls, the full consumer boundary was 2,794 polls, and the clock fired once at 2,131 between them. That fault returned `Resource(Timeout)` with zero callback calls and zero retained query bytes. The budget fault used 104,628 bytes, one byte above the 104,627-byte compiler peak and below the 344,571-byte full peak; it returned `Resource(Memory)` with zero callback calls and zero retained query bytes. Clean replays had three callbacks, zero fires, exact oracle equality, and query/shared/store release.

## Interface, static checks, and qualification boundary

The shared finalizer consumes the existing `LoweredRead` carrier, and `LoweredMutation` embeds it as `common`, exactly as the ZE-140 plan requires. The read route preserves exact `"read lowering clause"` and `"unexpected read clause"` diagnostics; the shared seam selects separate `"mutation lowering clause"` and `"unexpected mutation clause"` text for mutation. A neutral shared name would address the earlier low-level naming smell in isolation, but this ticket's explicit interface-preservation requirement controls the implementation, and repository standards defer to the ticket specification.

All three scoped `cargo clippy ... -- -D warnings` commands pass. `git diff --check` and the literal `rustfmt --edition 2024 --check` command over every owned Rust path pass. `cargo fmt --all --check` exits 1 and reports only the preserved pre-existing `crates/zeppelin-embed/src/property_graph/storage/records.rs` public-use ordering delta. That path is in the 57-entry inherited preservation manifest and remains byte-identical, so ZE-140 did not rewrite it. The exact raw output and exit for every one of these checks are in `final-command-raw.log`.

Hardware was an Apple M3 Max MacBook Pro (`Mac15,9`), 16 cores and 128 GB RAM, running macOS 27.0 build 26A5388g on arm64. Toolchain: `rustc 1.93.0`, `cargo 1.93.0`, and `cargo-nextest 0.9.145` default profile, `-j 4`, retries disabled.

This evidence proves complete validated compiler plans, exact mutation/source metadata, scoped ownership, controls, and additive PG21 coverage. It performs no graph mutation or identity allocation and proves no writer/base-view admission, physical eager drain, progressive stored-property execution, overlay staging, revision/fence/NoOp outcome, atomic commit, public ABI, reopen, TCK, Windows/native Intel, CI, or minimum-macOS behavior. ZE-52/57 retain runtime semantics and ZE-118 retains the broad workspace, adversarial, coverage, size, sanitizer, and platform campaigns. ZE-140 remains `in_progress` for independent final review and was not closed.
