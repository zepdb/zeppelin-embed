# ZE-37 integration on main

Integrated candidate `79a04dd922d82c8833a8ddde671a9caab9ca1e6e` over
`f9d81e0806ca68c876a26ed1983a3fffaf72b902`. Seven shared registration/documentation
conflicts retain every existing query, WAL and runtime export/probe. Five old
append-only files retain their complete main prefixes. All 25 candidate source
hashes match the independent reviewed snapshot; 17 integrated files are exact
candidate bytes. The other eight contain the reviewed union and two new focused
integration tests. Shared lifecycle/resource code remains exact ZE-49 main bytes.
All 45 pre-existing user-file hashes are unchanged. See source-audit.json and the
lossless candidate-to-main patch.

The new staged result test uses actual ZE-49 QueryMemory::adopt_shared for each
real core/ABI/registration reservation. It pins unchanged writer charges, only
three added query-control charges in aggregate accounting, the exact local query
charge, and complete release. Tightening the actual query budget rejects at each
of the three transfers and rolls back partially adopted ownership. Removing the
query backing charge delayed the intended rejection and fired the named assertion
(nextest exit100); the production file was restored byte-for-byte. Registration
remains the concrete synthetic fixture, not an actual FFI registry acceptance.

The new actual runner test removes the possibility that a passing standalone
probe hides absent runner wiring. Removing only the PG10 runner call fired the
missing-staging-coverage assertion (exit100). After exact restoration, seed0 ran
59 operations with zero violations and every PG10 key present. This is one
focused episode, not the full adversarial campaign.

Terminal main checks: **63 unique focused tests passed**: 56 public staging,
lifecycle and canonical tests; three internal allocation/framing tests; three
runner/probe tests; one primitive oracle test. Strict all-targets clippy for core,
oracle and workspace-tests with allocation-audit,test-support passed. Workspace
formatting and source diff checks passed. Exact commands and compressed raw logs
are in commands.json. Host/tool context is unchanged from ../host.json.

The first draft of the integration test hit a Rust borrow-check error because
the error result still retained its query loan; explicitly dropping that error
value fixed the test harness. This is not behavioral RED evidence. Both later
production mutation controls above failed at the intended runtime assertions.
No integration product correction was needed.

Core/ABI publication, GraphStore admission, WAL/recovery and actual binding
registry behavior remain their owning tickets. Broad workspace/adversarial and
whole-crate coverage qualification remains ZE-118 in Backlog; none is claimed
passed by this integration.
