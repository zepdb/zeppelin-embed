# ZE-43 PG8 independent review

## Disposition and source correspondence

**Accepted for the bounded native-directory PG8 adapter scope. No open concrete source/model blocker found.** This does not close ZE-43 or qualify public GraphStore admission, leases, publication, durability, recovery or the full adversarial matrix.

Reviewed `/tmp/ze43-pg8-review`: all **38** frozen hashes verified. The three included oracle source/test files are byte-identical to integrated ZE-121 commit `6fa8d5f`. Production dependencies are the accepted corrected COW snapshot; the sole additional semantic production API is the borrowed `RecordView::canonical_bytes()` getter, retaining the same immutable source/lifetime.

The final immutable delta `/tmp/ze43-final-review-delta` was reviewed and applied only to independent scratch: one needless borrow removed from `Store::open`, and comments in `PreparedObjects` clarify identity-only allocation callbacks, externally charged backing and writes-owned cleanup for paths created before a private pack exists. No behavior changed in that delta. The final composite remains 38 paths; every scratch hash matches the original manifest with exactly those two verified replacements. Full inventory: `/tmp/ze43-pg8-independent/final-receipt.json`.

## Expected state is independent

`fixture::trace(seed)` authors eight operations with explicit full-u128 node/relationship IDs, revisions, labels, endpoints, delete/recreate preconditions, exact NUL/Unicode key bytes, stored text and f64 bits. `Request::primitive()` and its manual `canonical()` byte builder supply the ZE-121 model directly. They do not call an engine canonical encoder, read a production result, use a production classifier, or derive expectations from a directory observation.

`Base::after` does copy actual staged deltas/canonical bytes and symbol allocations, but only into the explicitly fake retained staging-base/catalog adapter used for subsequent production requests. That state has no path into model operations or model canonical bytes. Engine IDs/symbol assignments that disagree with the authored primitives therefore fail comparison rather than changing expectations.

The observer walks actual production directory cursors in emitted order, decodes records/tombstones/fences through their checked native APIs using actual leaf generation, copies exact canonical/key/provenance bytes, and preserves label/type order. It does not sort, deduplicate or patch the observations. Current generation and every earlier saved root are compared against separately owned model snapshots after each clean generation. The bounded node seeks and exact fence point lookups are also checked. One fixed namespace intentionally aligns physical symbol ordering with logical namespace-byte ordering; this PG8 trace does not replace the separate full numeric namespace comparator tests.

## Actual storage and fault route

The adapter calls `stage_structured` → `prepare_directories` with authentic `WriteMemory`/`StorageMemory` → `PreparedObjects::finish` → actual temporary files → fresh `OwnedArtifact::read_from`. Candidate and private packs are dropped, and their capacity release is asserted, **before** reopening the files. Observation source resolution uses only newly admitted file buffers. The staging batch/base may still own their fixture canonical bytes, but the observer never reads from them as native artifact bytes.

Generation 4 targets first/middle/final observed source-resolution and append calls. `Read` and `Append` inject and require `TreeError::Missing`; `Cancel` sets the token on the scheduled append attempt before forwarding to the inner append and requires `TreeError::Control`; the explicit one-work-unit attempt requires `TreeError::Work`. A failed preparation must return no candidate and expose no finalized artifacts, preserve its private abort descriptor inventory, and release owned capacity on drop. These are controlled component faults, not claims about OS errno or uncertain durable outcomes.

For each seed, append/read/cancel each report three fire counts and three same-seed clean counts; work refusal reports one pair. Each clean rerun compares all eight observations exactly. A broken run's three already-validated prefix observations must match the clean prefix. Those prefix objects were collected before failure; this is not an additional post-failure file reread or crash-recovery assertion. Clean generation traces independently reread all saved roots after every successful change.

All 15 PG8 coverage names are additively present in the global smoke registry, the actual runner invokes the probe, and the focused runner test checks their receipt. Existing registered routes are retained by the diff. The component boundary remains explicit: authored staging base/catalog, no admitted public read view or coherent graph/search publication.

## Independent executed checks

Scratch: `/tmp/ze43-pg8-independent/repro`, based on `git archive 81e6e95` plus frozen files and the reviewed final delta. Isolated target: `/tmp/ze43-directory-independent/target`.

- Focused nextest: **2/2 GREEN, 454 filtered**, exit 0, after every control was restored. One test runs seeds **0 and 41**, the other calls the **actual runner episode**. Terminal raw output: `/tmp/ze43-pg8-independent/terminal-green.log`.
- Strict scoped `cargo clippy -p zeppelin-embed-workspace-tests --test adversarial_tests --no-deps -- -D warnings`: **GREEN, exit 0**, `/tmp/ze43-pg8-independent/terminal-clippy.log`.
- Five deliberately incorrect changes, each made only in scratch, each produced its intended runtime **RED, exit 100**, then was restored byte-exactly:

| Control | Observed failure |
| --- | --- |
| Alter bytes returned by the actual decoder observation helper | Generation 1 independent comparison rejected `nodes[0].provenance.key.namespace[0]`, expected 112, observed 113. |
| Make the actual reopened artifact source fail to find its objects | PG8 returned `missing graph directory artifact` through native observation. |
| Corrupt byte zero of the actual file written before fresh reopen | `OwnedArtifact` refused the native graph artifact's Magic, expected first byte 90, observed 91. |
| Omit the actual runner's PG8 invocation | The actual episode test failed on missing `property-graph.directories.native-history`. |
| Change production node deletion to omit `prepare_node_tombstone` and return no record | Generation 5 independent comparison rejected `node_tombstones.length`, expected 1, observed 0. |

The first four receipts/log hashes are in `independent-controls.json`; the production tombstone control has `production-mutant.json` and `drop_production_node_tombstone.log`. Final nextest/clippy commands, all log hashes, hardware/OS and the 38 final source hashes are consolidated in `final-receipt.json` in the same evidence directory. The built-in synthetic discrepancy checks separately test canonical bytes, original generation and observed fence order; the independent controls above reach actual source/file/runner and product paths.

An initial command was mistakenly launched from main while its owner was resolving an integration. That compile-only attempt exited 101, made no source writes, and is explicitly **excluded** from all RED/GREEN counts (`excluded-wrong-cwd.log`). All accepted review commands use the explicit scratch working directory.

## Evidence preservation and remaining work

Keep this report with `/tmp/ze43-directory-independent-review.md` and its three original COW REDs, plus `/tmp/ze43-cow-independent-review.md` and the restored COW checks. The PG8 review does not supersede those findings/closures. Final ZE-43 fuzz, allocation-audit packaging and commit integration remain owner work; the review did not repeat or claim those results. No real worktree, tracker or immutable snapshot was modified. No broad campaign ran; nonessential broad qualification remains ZE-118.
