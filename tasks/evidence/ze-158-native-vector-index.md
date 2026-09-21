# ZE-158 native vector source index evidence

Verified 2026-09-20 in the preserved ZE-158 worktree on Apple M3 Max,
arm64 macOS 27.0 (26A5388g), 128 GiB RAM; rustc 1.93.0,
cargo-nextest 0.9.145. These are local macOS results.

Source pin: `856cfa602742b4bd6eaefda14300c91e9f8753d8`.
Original producer commit: `6120701a3eef677828b7e05fe9cf2d98cc3e70dd`.
The results below distinguish that implementation from the reviewed corrections.

- Frozen plan `ze-158/astra-plan.md`, SHA-256 `a553608d2acedb9528b9188798e72d24a4c44d2d8109fcd9610662219b06b631`.
- High-ID addendum `ze-158/high-id-fixture-addendum.md`, SHA-256 `072154dd435f7c19b424dec6a57fdd854e2e3547d44bfd826b6f3db3ddf6a51c`.
- Frozen seeded-constructor patch SHA-256 `5640c4cfc6f92c78e1ec0f092bec3b106efd3dbbd90829412bd6e3847b3e405c`; `persistence.rs` is unchanged by this correction.
- Accepted correction plan `/tmp/ze-158-review-correction.md`, SHA-256 `b6d6122a3027cbac554a6e7e9feb575791566f5a73949fafb0a17d7273ceb07a`.

## Historical producer evidence

The first behavioral RED, recorded as run `6413658a...`, reached an ordinary
native write/admitted source, then failed because its native index was absent.
Earlier setup failures are not behavioral RED. First useful GREEN
`d898216b-c586-4c43-b71e-ad564f15a99f` published a V2 source/role-18 index and
invoked the real Bit4 estimator and GraphSearcher over persisted bytes.

Original terminal runs were `780ff8a4-3163-41b2-aba8-5bbe805a7f76` (8/8 native,
667 skipped) and `88997877-cd9d-463b-95ee-0fa5950d186e` (5/5 shared, 670 skipped).
The original core/FFI checks and graph clippy passed. Those narrower tests did
not establish all reviewed requirements: new Text V2, self/duplicate-edge and
profile/norm rejection, authentic in-flight native controls, exact physical
read receipts, and the full shared runner registration needed corrections.
The original isolated callback/pre-cancelled tests are not native in-flight
close/deadline/memory/work-limit evidence.

The original retained-read fixture exceeded the existing resident allowance
with a 64 MiB query reservation; it was corrected to the established 8 MiB
fixture reservation. The original extent fixture failed at 256 and 192 rows
by two dimensions with `Read(Work)`; 24 rows by 512 dimensions produce a genuine
extent under the unchanged limits. An `errno=28` linker failure was resolved
by cleaning only this worktree's package build artifacts. None widened a
production limit. The later directed traversal uses the existing
`MAX_QUERY_BYTES` contract rather than its insufficient 32 MiB fixture limit.

## Corrected producer and validation contract

Every nonempty new vector source, including 1/2/3-row sources, owns an index
built with shared Bit4, Vamana and GraphNodeBlocks algorithms. Preparation uses
changed-row cohorts bounded by 1,024 rows and 4 MiB of canonical coordinates,
subject to existing actual memory/work controls. Queries do not build indexes.
The 256-byte header binds full node IDs/revisions, codes, factors, original f32
bits, graph bytes and the historical build catalog. No legacy publication path
or duplicate quantizer/builder is used.

New logical **Text and Vector sources explicitly select V2/200 bytes**; Text
has no vector index and Vector requires one. Historical V1/144-byte physical
rewrites remain V1 and raw. Physical row rewrites preserve the index descriptor
and use the genuine catalog through a copied immutable overlay. Outer
PhysicalRef, participant and root versions remain 1. This does not qualify
ZE-46's actual relocation/reclamation workflow.

Admission validates header/version, contiguous section geometry and trailing
bytes, catalog interpretation/profile, seeds, self/duplicate/out-of-range
edges, codes/factors, full row identity/revision, original coordinates and
Angular unit norm. Unit mutations repair relevant framing/checksums before
asserting semantic rejection; original valid bytes remain accepted. Seed
checks prove exact `min(4,N)` flagged entries and header/flag agreement, not an
independent reimplementation of the seed-selection algorithm.

The high-ID fixture writes H-1 and H where H=2^80, observes original coordinate
bits and real kernel identity mapping, writes relationship 2^96+7, reopens,
and allocates H+1. This is a genuine allocator crossing; it does not prove an
identical-low-64 collision. Source isolation retains a real old admission over
update/delete, preserves old identities/fingerprints, observes revision 2 in a
fresh admission and excludes the deleted identity.

Bounded control is proved for native preparation's validation, quantization,
query preparation, graph work/output validation, initialization, serialization
and checksum leaves. Work is polled inside chunks of at most 256 coordinates
or bytes. Actual capacity ownership remains charged. This is not broad query
traversal/cancellation qualification. Sequential f64 accumulation is retained
across chunk boundaries; public invalid-input validation occurs before scratch
allocation. Fallible graph/query-code allocation errors retain their existing
Store allocation cause and map to native `TreeError::Memory`, while geometry
errors remain distinct.

## A–F review-correction runs

All native single-group commands use:

```sh
cargo nextest run -p zeppelin-embed --lib --features graph-cypher -j 4 --retries 0 -E 'test(native_vector_index_NAME)'
```

The table supplies the NAME suffix after `native_vector_index_`. GREEN single-group
runs selected 1 test with 674 skipped unless stated otherwise. New successful
coverage is not retroactively labelled a production RED.

| Correction / NAME | Recorded RED or correction stop | GREEN run |
| --- | --- | --- |
| A and F: `identity_space_and_geometry`, structural/profile/norm rejection and explicit new Text V2 | `8b9c479c-fdc3-476a-a413-46fc60a9b0b5` | `e7291697-3893-4a10-b560-ab339ed741b7` |
| B: `prepare_limits_controls_release`, bounded work/numeric/control assertions | Earlier reviewed missing polls; no separate invented RED ID | `16ca6065-c9ce-4010-9748-ed3e807377df` |
| C: `prepare_limits_controls_release`, real acknowledged Closing during Build | `59a3a8cd-0707-4711-bbf5-84f2c9604b7e`, expected typed ReadCancelled did not match | `648cbee6-a8e2-4829-9a57-8969c6d471e1` |
| D: `identity_space_and_geometry`, genuine high-ID/physical-overlay and geometry cases | Fixture compile/query/filter assumptions corrected; not counted as producer RED | `9d15dfc9-f75d-4ef2-aed0-a768f4d7e063` |
| D: `publication_reopen_required_refs` | New successful WAL/checkpoint/missing-reference/VFS coverage | `957fd7fa-0314-40e0-9717-910767a065cb` |
| E: `quantizer_builder_reuse`, actual native/legacy bytes and literal-distance comparison | New independent producer comparison | `72a8faca-f906-496f-bdd4-65d06221602d` |
| E: `directed_probe_can_fire`, actual trace/traversal plus restored comparator controls | Fixture memory/expected-reference multiplicity corrections | `2584771d-b767-47aa-a628-fba26031e322` |

The C shared fixture uses four identical 513-coordinate rows. Clean observed
stages derive the work and memory denial limits; all schedules fire. Real
QueryControl cancellation and manual deadline produce typed failures. A real
close thread reaches StoreState::Closing before the held Build barrier is
released; the writer aborts with ReadCancelled and close drains. Generation
and sequence remain unchanged. Non-close failures release to their exact
pre-operation reserved-byte baseline; closed ownership is compared with an
identically closed clean control. No separate cancellation mechanism or
close/drain/commit-order change was introduced.

D proves genuine WAL-only and explicit-checkpoint reopen with identical index
bytes/source observations and zero preparation events. Missing extent chunks
and historical catalogs reject in both paths. The existing object-sync VFS
fault fires once after real preparation, leaves state unchanged, and its
same-input clean control publishes successfully. Corruption/missing-artifact
matrices remain unit-only.

E compares actual native and legacy codes, factors, graph bytes, entries and
candidate identities/distances on the same cohort. Trace uses an older source
catalog plus a real 24x512 extent source; capacity-one and capacity-256 batches
complete in identical order, cover required descendants and release owners.
Foreign resources, cancellation and late missing descendants are unit-only
negative cases. The directed comparator observes six deliberate failures
(missing/duplicate reference and altered receipt/value/seed observations),
restores each, and records two actual trace-owner releases. Catalog occurrence
counts account for the three authentic references in that fixture; they are
not disk-read counts.

## G: shared leaves and actual registered consumers

Shared helper bodies live in `vector_index/test_support.rs`; scratch directories
use std only. Large unit matrices remain under cfg(test). The hidden export
requires graph-cypher + test-support and does not expose a shipping API.
Each consumer validates its independent report before crediting only its key:

| Exact key (prefix `property-graph.native-vector-index.`) | Shared extraction GREEN |
| --- | --- |
| `kernel` | Existing single-write helper, latest terminal run below |
| `small-writes` | `11df40a2-0bb8-45f4-9b64-9e870611989c` |
| `identity` | `b17a3ad0-ec9c-4f98-98dc-bb280a12f52b` |
| `limit.fire` and `control.fire` (separate reports, shared clean setup) | `7f78240e-49c8-4bc9-a871-ee3e7e718a2f` |
| `reopen` | `6d272570-fc22-40b5-9194-1a936ab31246` |
| `trace` | `51060cf6-6a99-4ed2-99df-ab2ac16c0341` |
| `oracle.can-fire` | `4e30304c-a4eb-4d55-8321-f0353a25cfbf` |

Each extraction run selected 1/1 with 674 skipped; core graph+test-support
compilation also passed. Logs are `/tmp/ze158-astra-*-{check,test}.log`.
The temporary kernel-only registration is replaced by all eight keys in the
module, runner, coverage registry and direct consumer test. The standalone branch carried assertions of 225 graph keys / 245 with
graph-result-test-support, but those literals were stale: source enumeration
later found actual standalone counts of 256/276. Compilation did not execute
or verify those assertions. The main-based integration below corrects them
against the full union; the default 88-key inventory remains unchanged.

The first eight-test post-extraction run `0b6efb6d-4814-4e5d-84d9-0b3c92c2ce4f`
passed 7/8: the fixture selected the first source after additional publications.
It now selects the unique one-row source with full `(node, revision=1)`;
missing/duplicate targets fail. Literal kernel assertions stayed unchanged.
Rerun `ee620603-146f-4858-8440-e654deac32e6` passed 8/8, 667 skipped.

## Follow-up RED/GREEN proof from independent review

Arithmetic, allocation-order and allocator-cause tests used:

```sh
cargo nextest run -p zeppelin-embed --lib --features graph-cypher,allocation-audit -j 4 --retries 0 -E 'test(NAME)'
```

NAME is the exact full test name below; each selected 1 test, 691 skipped.

| Test / cause | Intended RED | Restored GREEN |
| --- | --- | --- |
| `vamana_build_is_deterministic_under_seed`: actual 513D [2^27,1,...,1] row and zero row; sequential distance/query norm | `ccce3dec-5d0b-41f0-8a89-75e1a99f4dc2`: both observed `0x4350000000000040`, expected `0x4350000000000000` | `1351287b-25ad-47ff-b12a-cafed26788e3` |
| `surviving_quantizers_reject_dimensions_beyond_i8_kernel_limit`: only public quantize_bit4 inside audit | `ec0e9255-8c7e-4c72-b79a-51e072540b52`: 2 allocation calls instead of 0 | `76ac40fc-6c25-40ab-ac61-d92c00702f96`: 0, sentinel/error contract preserved |
| `native_vector_index_prepare_limits_controls_release`: actual first graph reservation denial | `c59191ae-b0af-4b8f-b452-803ab4f43330`: generic Invalid instead of Memory | `8c3a50d0-6341-4c1c-80ba-76cf76ff7ddf`: 1 actual fire, unchanged state and exact release, clean write succeeds |
| `vamana_build_is_deterministic_under_seed`: actual query-code reservations 1 and 2, fixture allocations outside audit | `6eb62970-640f-4fc3-9993-f1918e885275`: both fires=1 but wrong allocation cause | `be6f4010-81ff-4372-8aa9-70de0ffdf7aa`: Store AllocationFailed, needed=513; restored literal distance |

The bounded four-test regression filter was
`test(bit4_rejects_non_finite_values_without_writing) | test(bit4_golden_fixture_is_stable) | test(native_vector_index_quantizer_builder_reuse) | test(native_vector_index_prepare_limits_controls_release)`
with graph-cypher, -j4, --retries0: runs
`de1d1826-a192-4615-9b69-b3699e68eb83` and
`b8269dba-96e9-4a44-b65d-32e9fa03e8b8` each passed 4/4, 671 skipped.
Raw logs use `/tmp/ze158-{arithmetic,validation-order,allocation-cause}-*.log`.

Finite-input validation used `native_vector_index_prepare_limits_controls_release`
with graph-cypher: 513 coordinates, NaN at index 512. Real cancellation at the
second quantizer validation callback and real deadline advancement at the
second query-preparation callback were required. RED
`1e8a6f22-8a58-469a-9bdb-a9df1f5de931` observed no callbacks/fires. Bounded
validation now observes `[256,256,1]` on paired clean NonFinite controls;
controlled cases fire at callback 2 with typed causes before the NaN scan.
Intermediate run `c060a60e-b036-4334-b3bf-35c1b5ee7f90` failed only an early
fixture close changing the resource baseline; removing that premature close
preserved the final close and all assertions. GREEN
`cd1b7a43-16dd-4ac1-9b30-b02395f2a132` passed 1/1, 674 skipped. Logs:
`/tmp/ze158-validation-polls-{red,green,green-rerun}.log`.

## Exact physical-reference resolution receipts

Scoped test/test-support observers record **successful PhysicalRef resolutions**
at native query/preparation decode seams after required-reference validation.
These are **resolution counts, not disk-I/O counts**: cached mappings can still
resolve a block. Expected source/index refs are bound independently from
admitted descriptors before measurement. Fixture descendant enumeration is
excluded; the actual index opening and kernels remain measured.

| Measured operation | Exact successful resolutions / events |
| --- | --- |
| One-row kernel query | 1 source manifest + 1 direct index |
| Four singleton writes plus one 32-row cohort, then inspect five sources | 5 source manifests + 5 direct indexes, one per bound pair |
| Existing indexes during each of those five creates, preparation origin | `[0,0,0,0,0]` |
| Newly prepared images for those five creates | `[1,1,1,1,1]` |
| Preparation events during the measured query | 0 |

Exact command:

```sh
cargo nextest run -p zeppelin-embed --lib --features graph-cypher -j 4 --retries 0 -E 'test(native_vector_index_single_write_real_kernels) | test(native_vector_index_small_writes_and_source_isolation)'
```

Instrumentation RED `0a418be6-9a2d-46db-845a-85f765ee0506`: 0/2, 673 skipped;
both queries recorded zero counts with decode observers deliberately unwired.
After wiring, one extra real index opening in the unit fixture caused exact-count
RED `45fab2b3-5503-4ac4-982a-e7d527d3adb1`: 1/2, 673 skipped; small-writes had
source counts `[1;5]`, index counts `[2,1,1,1,1]`, total 6 instead of 5.
That temporary opening was removed. GREEN
`4da2bca2-3ce1-4968-a6ba-06e06053abf4`: 2/2, 673 skipped, 0.635s.
Logs: `/tmp/ze158-physical-reads-{instrumentation-red,extra-opening-red,green}.log`.
Shared reports/hidden validators retain these independent refs and actual
counts. No counts were substituted for failed assertions.

## Historical isolated-worktree terminal gates

These ran sequentially after all source corrections and all eight registrations;
none executed an adversarial consumer or suite.

```sh
cargo nextest run -p zeppelin-embed --lib --features graph-cypher -j 4 --retries 0 -E 'test(native_vector_index_)'
cargo nextest run -p zeppelin-embed --lib --features graph-cypher -j 4 --retries 0 -E 'test(bit4_golden_fixture_is_stable) | test(vamana_build_is_deterministic_under_seed) | test(graph_build_handles_the_minimum_shape_and_rejects_an_empty_segment) | test(graph_search_small_persisted_seed_sets) | test(an_unfiltered_traversal_is_byte_identical_with_the_mask_parameter_absent)'
cargo check -p zeppelin-embed --lib -j 4
cargo check -p zeppelin-embed --lib -j 4 --features graph-cypher
cargo check -p zeppelin-embed --lib -j 4 --features graph-cypher,test-support
cargo check -p zeppelin-embed --lib -j 4 --features graph-cypher,allocation-audit,query-timing
cargo check -p zeppelin-embed-cypher --lib -j 4
cargo check -p zeppelin-embed-ffi --lib -j 4
cargo check -p zeppelin-embed-ffi --lib -j 4 --features graph-cypher
cargo clippy -p zeppelin-embed --lib -j 4 --features graph-cypher
cargo check -p zeppelin-embed-workspace-tests --test adversarial_tests -j 4
cargo check -p zeppelin-embed-workspace-tests --test adversarial_tests -j 4 --features graph-cypher
cargo check -p zeppelin-embed-workspace-tests --test adversarial_tests -j 4 --features graph-result-test-support
```

Native run `f002dfda-6819-4427-93d0-c419991616fe`: **8 passed, 667 skipped**,
2.587s. Shared run `63c08172-8c14-4301-9355-c1f6e9baf774`: **5 passed,
670 skipped**, 0.128s. Every following check and clippy exited 0 (warnings
remain). Logs: `/tmp/ze158-final-{native,shared,check-default,check-graph,check-support,check-audit,check-cypher,check-ffi,check-ffi-graph,clippy,consumer-default,consumer-graph,consumer-graph-results}.log`.

Scoped rustfmt over 26 owned Rust files changed no bytes after these gates.
`git diff --check` passed. The six inherited files match
`/tmp/ze-158-preservation.json`; `.agents`/tracker/CLAUDE symlinks and all
inherited dirty changes are preserved and excluded from the correction commit.
No Cargo manifest/lock/dependency change exists. Production/fixture and
registered-consumer source hashes follow; runtime scratch artifact bytes were
compared within tests, but standalone artifact SHA-256 values were not emitted.

## Qualification and root integration boundary

The named producer, validation, native failure, reopen, trace and comparator
fixtures pass, and all actual registered consumers compile. This does not
qualify the adversarial runner at runtime. Broad/full/advanced/adversarial,
coverage/fuzz/performance/release suites remain deferred through ZE-118/ZE-161.
No ZE-62 global/constrained ranking or recall, hybrid ranking, ZE-40 full
recovery, identical-low64 identity/binding parity or ZE-46 reclamation result
is claimed. Root owns tracker closure, main integration and explicit ZE-46
reconciliation of the minimal `native_graph/write.rs`/TreeResources lifecycle
checkpoint, the frozen seeded-constructor overlap, source V1/V2 and descendant
references. No push or tracker mutation was performed here.

## Historical isolated-worktree source SHA-256 inventory

- `crates/zeppelin-embed/src/graph/block.rs`: `bf06ad1c728f28b96b81362b5f4575e58bffd6d07a5222a7a4cec61e99bd05ef`
- `crates/zeppelin-embed/src/graph/build.rs`: `e88cf0d25e2f9a1b15c1fbe94a6e2f56dfd6e8730beed6d475003433c5643072`
- `crates/zeppelin-embed/src/graph/search.rs`: `6f3296d65ddaf98fcf92b2db9ec1c4677d7c3e03cf8c08f38664f3def86852d7`
- `crates/zeppelin-embed/src/lib.rs`: `b284623f87984d3a2725b70e226e72fe48e327d55f0556f68b94aa68b0b0e920`
- `crates/zeppelin-embed/src/lifecycle/native_graph/tests/publication.rs`: `5c44aa439d3fa992ec4bf88f2221802f94163e2b5adaaf94d060e78f3f7d0d7b`
- `crates/zeppelin-embed/src/lifecycle/native_graph/write.rs`: `9f15098ca4a9f3707acbbb36971ab66b3fbf1226a7878db8183194fed420c00e`
- `crates/zeppelin-embed/src/property_graph/storage/search.rs`: `ab6931552deff1dbd475f932cfe73cb8dbd3d32e1b7fbab56d158591295132dd`
- `crates/zeppelin-embed/src/property_graph/storage/search/prepare.rs`: `67f93a99644ad8d40ab5ebb26fffca8e3bc6fdcdedc630b76520cee281b6fdfc`
- `crates/zeppelin-embed/src/property_graph/storage/search/vector_index.rs`: `0fa252d38f33c688422489b1ee75f971979244b58d2038fab734e3d7cedd79db`
- `crates/zeppelin-embed/src/property_graph/storage/search/vector_index/prepare.rs`: `31997375e0d5b112c67d3953eaf0e046949b9aeef2e41386119865091c92066d`
- `crates/zeppelin-embed/src/property_graph/storage/search/vector_index/tests.rs`: `f4768c31cd387a20c0840d78fe3ea2b4e7c6a8c406384850d7c2f0147297e176`
- `crates/zeppelin-embed/src/property_graph/storage/search/view.rs`: `771b587d06b35d73bfbbd1c54694ee51ca5c0184f6929ab1bf680f32c246bc80`
- `crates/zeppelin-embed/src/property_graph/storage/tree/directory.rs`: `91baee977323b65c56b00caf69d076e000c4a110229b0896280afe5a5302c2cf`
- `crates/zeppelin-embed/src/property_graph/storage/view/preparation_source.rs`: `89b1d325638c9168198856dfb4c8be1852e650452fa74d0caa66b64dbf32a56e`
- `crates/zeppelin-embed/src/property_graph/storage/view/source.rs`: `613eebd071a57633bfe5355afe7b84f4d0f94d6f7f588d11e6e7a74003685a99`
- `crates/zeppelin-embed/src/quant.rs`: `9273088d1feb4647d2ebdcff597f8c81153cb0cbd4cad4415da270b18a4d384c`
- `crates/zeppelin-embed/src/quant/bits4.rs`: `bb9186f712ff7342c4410928cf2ab6d32d3c8890a77d4feaf239914bb2ab13bc`
- `crates/zeppelin-embed/src/quant/bits4/controlled.rs`: `cfefd2757dc255e5f3c43cf8afdac275f89a53959a5ccae7990211fd5b296a7a`
- `crates/zeppelin-embed/src/quant/tests.rs`: `d347160cceb6bf7bee99420fa5db1d058d590f7999fdebc046851ff7a4051ef3`
- `tests/adversarial/coverage.rs`: `60cdce645aacdc915de2e66bd638026d62470156e3c3dec9b29ef77753b2bc81`
- `tests/adversarial/mod.rs`: `ad53c009b757886c589a9fa1948d0d0f3ad1beb1ca747650c2ecfba26a1a6a45`
- `tests/adversarial/runner.rs`: `4cab6fe5e0f3e2d5d99e92d3ff2bec70fe1724453432e87c1ce46eafa3946432`
- `tests/adversarial/vector_execution.rs`: `97bb75ed67d72dff007b837b53e51f7e6eb521bc46985867196f89558c2ddbf4`
- `tests/adversarial_tests.rs`: `db61afcbf9352cb02aa5bbec9ce8e063f497b34caedfbd102374584112a7aa4b`
- `crates/zeppelin-embed/src/property_graph/storage/search/vector_index/test_support.rs`: `e3bd8a877e2aa70a950c4d209a94b9cbf4aa5a5de8e890e2a9cad5d15f5f206d`
- `tests/adversarial/graph_native_vector_index.rs`: `36a08db735283fb8bfbbe392063c63a7ac584036e205d9735f434782bf3904c7`

## Main-based integration on a9d22ee

Integrated in `/Users/aghatage/Documents/code/zeppelin-embed-wt-ze-158-main`,
branch `codex/ze-158-main-integration`, based on
`a9d22ee1ce543c9f39ba46d580655a5f2cac859b`. This carries the combined corrected
producer from original `6120701a3eef677828b7e05fe9cf2d98cc3e70dd` and correction
`9e2d1c65afe29cf970544eb1418dff6143b50f23`; their source pin and historical
RED/GREEN proof above remain unchanged.

The port checked current-main blobs against `856cfa6` before copying 30 final
paths. Three shared registration files were merged additively. The allocator
seed constructor and frozen Astra plan already matched the final branch and
were not replaced. A 69-file preservation inventory covers current-main
ZE-154/156/160 files and inherited project files. All hashes remain unchanged,
including relational/result producers, native record properties, admitted-view
cancellation and the ZE-160 close-owner fix. Root main and tracker were not
modified. The new branch has 33 owned changed paths, listed separately in
`/tmp/ze158-main-owned-paths.json`.

The source inventory audit established 268 unconditional graph keys plus 20
result-support keys on current main, all unique. Adding the exact eight
ZE-158 keys gives **276 graph / 296 graph-result-support**; the literal
assertions now match. Every previous main key remains present. Default
coverage stays **88**. This is static source verification plus consumer
compilation, not execution of the adversarial inventory test or runner.

The first gate was:

```sh
cargo check -p zeppelin-embed --lib -j 4 --features graph-cypher,test-support
```

It exited 0. The two native/shared nextest commands in the historical terminal
section were then run unchanged. Existing main integration controls were:

```sh
cargo nextest run -p zeppelin-embed --lib --features graph-cypher -j 4 --retries 0 -E 'test(native_relational_)'
cargo nextest run -p zeppelin-embed --lib --features graph-cypher -j 4 --retries 0 -E 'test(native_result_)'
cargo nextest run -p zeppelin-embed --lib --features graph-cypher -j 4 --retries 0 -E 'test(native_close_drain_releases_last_temporary_owner) | test(native_close_best_effort_releases_last_temporary_owner)'
```

| Main-based selection | Run ID | Passed | Skipped | Elapsed |
| --- | --- | ---: | ---: | ---: |
| Native vector | `7e760180-edd1-40bf-89d4-1013887f6131` | 8 | 686 | 2.720s |
| Shared kernels | `169639e8-1995-43d6-bb6e-9c357f199a5d` | 5 | 689 | 0.143s |
| Native relational | `914dca5f-2b62-4ac4-8858-e1dc9ea6adb3` | 8 | 686 | 0.686s |
| Native completed result | `dd247c11-75a2-4e1f-adf5-faa1d2d73491` | 8 | 686 | 0.661s |
| Native close-owner controls | `03fe5d81-5462-4878-8f38-e6d98b7fe72f` | 2 | 692 | 0.047s |

All remaining exact core/FFI/cypher feature checks, graph clippy and the three
registered-consumer compile commands from the historical terminal section ran
sequentially and exited 0; the already-passing first support check was not
repeated. Warnings remain. Raw logs are
`/tmp/ze158-main-{native,shared,relational,result,close,check-support,check-default,check-graph,check-audit,check-cypher,check-ffi,check-ffi-graph,clippy,consumer-default,consumer-graph,consumer-graph-results}.log`.
No failed gate occurred during this integration.

Scoped rustfmt covered 31 owned Rust files; only declaration ordering in the
shared adversarial module list changed. No module or registration was removed.
`git diff --check` passed. Preservation and exact path review passed. The
minimal write/resource lifecycle checkpoint remains a direct ZE-46 overlap
for root reconciliation; successful focused close tests do not qualify
relocation/reclamation. All broad/full/adversarial/performance/release
execution remains deferred through ZE-118/ZE-161.

### Main-based source SHA-256 inventory

- `crates/zeppelin-embed/src/graph/block.rs`: `bf06ad1c728f28b96b81362b5f4575e58bffd6d07a5222a7a4cec61e99bd05ef`
- `crates/zeppelin-embed/src/graph/build.rs`: `e88cf0d25e2f9a1b15c1fbe94a6e2f56dfd6e8730beed6d475003433c5643072`
- `crates/zeppelin-embed/src/graph/build/native.rs`: `d51704c4a8a5b56302e170711bf48c3ccadf902943dacdb356c540f5c4df0e49`
- `crates/zeppelin-embed/src/graph/search.rs`: `6f3296d65ddaf98fcf92b2db9ec1c4677d7c3e03cf8c08f38664f3def86852d7`
- `crates/zeppelin-embed/src/lib.rs`: `b284623f87984d3a2725b70e226e72fe48e327d55f0556f68b94aa68b0b0e920`
- `crates/zeppelin-embed/src/lifecycle/native_graph/tests/publication.rs`: `5c44aa439d3fa992ec4bf88f2221802f94163e2b5adaaf94d060e78f3f7d0d7b`
- `crates/zeppelin-embed/src/lifecycle/native_graph/write.rs`: `9f15098ca4a9f3707acbbb36971ab66b3fbf1226a7878db8183194fed420c00e`
- `crates/zeppelin-embed/src/property_graph/storage/artifact.rs`: `b75a34adf54f68a0e363d49a398bbfc9d3471e1e56725c2b1369290cf05270fc`
- `crates/zeppelin-embed/src/property_graph/storage/payload.rs`: `56ccf9808a2cf7fb9f264fa018e919f017926afc6bd9eb7c25e57c0b7d8ec277`
- `crates/zeppelin-embed/src/property_graph/storage/search.rs`: `ab6931552deff1dbd475f932cfe73cb8dbd3d32e1b7fbab56d158591295132dd`
- `crates/zeppelin-embed/src/property_graph/storage/search/codec.rs`: `cb311e0780c82d9b9afee09bef0cd7daf3a25e791be1da1695ab96acdb7c0950`
- `crates/zeppelin-embed/src/property_graph/storage/search/prepare.rs`: `67f93a99644ad8d40ab5ebb26fffca8e3bc6fdcdedc630b76520cee281b6fdfc`
- `crates/zeppelin-embed/src/property_graph/storage/search/trace.rs`: `c1d38564b518bd047d9d2a59b7d5d12324df408faca93b79dbdd5c424fd81e02`
- `crates/zeppelin-embed/src/property_graph/storage/search/vector_index.rs`: `0fa252d38f33c688422489b1ee75f971979244b58d2038fab734e3d7cedd79db`
- `crates/zeppelin-embed/src/property_graph/storage/search/vector_index/prepare.rs`: `31997375e0d5b112c67d3953eaf0e046949b9aeef2e41386119865091c92066d`
- `crates/zeppelin-embed/src/property_graph/storage/search/vector_index/test_support.rs`: `e3bd8a877e2aa70a950c4d209a94b9cbf4aa5a5de8e890e2a9cad5d15f5f206d`
- `crates/zeppelin-embed/src/property_graph/storage/search/vector_index/tests.rs`: `f4768c31cd387a20c0840d78fe3ea2b4e7c6a8c406384850d7c2f0147297e176`
- `crates/zeppelin-embed/src/property_graph/storage/search/view.rs`: `771b587d06b35d73bfbbd1c54694ee51ca5c0184f6929ab1bf680f32c246bc80`
- `crates/zeppelin-embed/src/property_graph/storage/tree/directory.rs`: `91baee977323b65c56b00caf69d076e000c4a110229b0896280afe5a5302c2cf`
- `crates/zeppelin-embed/src/property_graph/storage/view/preparation_source.rs`: `89b1d325638c9168198856dfb4c8be1852e650452fa74d0caa66b64dbf32a56e`
- `crates/zeppelin-embed/src/property_graph/storage/view/source.rs`: `613eebd071a57633bfe5355afe7b84f4d0f94d6f7f588d11e6e7a74003685a99`
- `crates/zeppelin-embed/src/quant.rs`: `9273088d1feb4647d2ebdcff597f8c81153cb0cbd4cad4415da270b18a4d384c`
- `crates/zeppelin-embed/src/quant/bits4.rs`: `bb9186f712ff7342c4410928cf2ab6d32d3c8890a77d4feaf239914bb2ab13bc`
- `crates/zeppelin-embed/src/quant/bits4/controlled.rs`: `cfefd2757dc255e5f3c43cf8afdac275f89a53959a5ccae7990211fd5b296a7a`
- `crates/zeppelin-embed/src/quant/tests.rs`: `d347160cceb6bf7bee99420fa5db1d058d590f7999fdebc046851ff7a4051ef3`
- `tests/adversarial/coverage.rs`: `726e1eb4856fb3e6f7390ab7542a1a791bd888c3b630c405ef542a1460f72509`
- `tests/adversarial/graph_native_vector_index.rs`: `36a08db735283fb8bfbbe392063c63a7ac584036e205d9735f434782bf3904c7`
- `tests/adversarial/mod.rs`: `d4ab78081c3228c558065f533d94fe9763df9cc9d34a5b42a7908404557e5161`
- `tests/adversarial/runner.rs`: `a65aedd1645d3d5dcbd4773d18514bbee8087cbe213aadbdad2ee31c66df3366`
- `tests/adversarial/vector_execution.rs`: `97bb75ed67d72dff007b837b53e51f7e6eb521bc46985867196f89558c2ddbf4`
- `tests/adversarial_tests.rs`: `1dc5358ec52c099310ecbc4fa62cd2b8fc35a02cb0281e744c5cad2c51cc52eb`
