# ZE-126 main integration

Candidate `d2e36c7655ee95ccdbc8fa891da93b2294d9531b` is integrated onto main base `0e0064ddb3b1630e8c18050cb78c30ac7bcfb638`. The candidate parent import is not cherry-picked. All 36 source and 90 listed evidence hashes were checked against committed candidate bytes before integration; the evidence manifest itself makes 91 evidence paths.

The 127 candidate paths are retained: 121 byte-exact, five additive shared registrations, and one lint-only runner change. Three conflicts were resolved from complete committed main files plus exact candidate append sections, never by unioning conflict fragments. The two other registration files merged cleanly. Full candidate-to-integrated hashes and raw diffs are retained. Existing PG8/PG14/PG15/PG16 modules, tests, feature gates and runner calls remain, with PG17 added once. The inherited 45 instruction/context/dirt file hashes match the current pre-integration baseline.

Main focused nextest passed 29 frontend tests, 65 core tests and seven graph-enabled runner tests. PG17's real episode reports 59 operations and zero violations. Each nextest test runs in its own process, four processes concurrently, retries zero, one libtest thread per process. These are focused integration checks, not a full adversarial campaign.

Strict combined runner lint initially failed on `needless_range_loop` in the new PG17 coverage registration. The correction explicitly enumerates the same ordered five keys; case IDs, coverage keys and all oracle/fault/control behavior are unchanged. Strict lint then passes, as do both affected PG17 tests and formatting. The original failed lint log is retained. Candidate scoped core/frontend lint results remain valid for byte-identical production sources; no whole-workspace lint claim is made.

The final independent integration review is `sol-review.md`. It checks source preservation and the actual existing owner/detach seam, without replacing the candidate's independent semantic, resource/control and PG17 reviews.

Exact commands, exit codes, elapsed times, preservation and correspondence are in the adjacent JSON files. `raw-integration.tar.gz` retains all exact integration logs, diffs, command records and review bytes. SHA-256 inventory pins the archive and copied readable records. Root closure records the final main commit in the tracker and ZE-118.

Actual graph execution, original read TCK, public lifecycle, compaction/reopen and final shipping qualification remain on their existing tickets. Broad workspace/adversarial/coverage/size suites remain deferred through ZE-118 and have not been claimed passed.
