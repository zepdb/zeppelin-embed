# ZE-37: bounded private graph write staging

Date: 2026-09-19. Candidate base: `cab33b758c405ba73e0785841147ecdd480d7cd6`.
Worktree: `/Users/aghatage/Documents/code/zeppelin-embed-wt-ze-37`.
Branch: `codex/ze-37-write-staging`.

The implementation prepares complete logical graph/search participant deltas
against one retained store/generation/root base. It has read-only base access and
no publication capability. It checks all request metadata, payload fields,
canonical/provenance input framing, Restrict integrity and complete output
layout limits before selecting a changed generation or assigning private IDs.
Resolved fresh endpoint bytes are encoded after private node assignment.
Rejection returns no batch, receipts or high-waters and releases preparation.

## Scope and interfaces

- Structured requests reuse the existing key lifecycle rules, uniformly reject
  duplicate keys/resolved entities, preserve each replay's original generation,
  and allocate generation/IDs/symbols only for changed work. Dynamic canonical
  read-at sources avoid forcing a whole base-image copy for exact comparison.
- The Cypher overlay serves pending property/text access only. Absence, empty
  text and deleted access remain distinct. Final equality is NoOp; changed
  revisions advance once. Unkeyed create/delete retains consumed high-waters
  without a final entity row, receipt or symbol allocation.
- ZE-109 DETACH produces one node tombstone and its search-membership removal;
  it never asks for incident enumeration. Restrict probes alive incidents,
  excluding validated explicit relationship deletes. Admitted providers must
  enforce both endpoint liveness and coherent records from the retained base.
- `WriteMemory` uses the existing Store's shared Accounting owner, keeps the
  writer's 64 MiB sublimit, reserves before allocation and reconciles actual
  Vec capacity. Framed canonical plus provenance input is capped at 8 MiB;
  entity changes at 16,384 and result rows at 65,536.
- Result materializers preflight the actual pending receipt count, separate
  4 MiB core and 4 MiB ABI capacities, and additional registry/control capacity.
  Checked known simultaneous writer capacity is validated before IDs. The exact
  selected layout is retained for subsequent preparation, with no fabricated
  identities or later layout recomputation. Allocator failures and actual
  capacity reconciliation remain fallible private Prepare work.
- Materialization retains the real supplied registration token. The consuming
  result-owner adapter moves each sole shared reservation while preserving its
  writer-local guard. Backing and registration drop before capacity owners.
  Synthetic registry/query owners prove this capability's rollback and ownership;
  they do not stand in for a real FFI or query-runtime integration test.

Authoritative shared ZE-49 `resources.rs`, lifecycle `budget.rs` and `stats.rs`
are byte-identical in both worktrees; see [shared audit](ze-37/shared-source-audit.json).
Root should retain one copy when integrating. The public GraphResources route
borrows an actual Store's existing Accounting Arc, not a standalone graph owner.

Pinned Kuzu reference `89f0263cc7a1fd9c396d2c4953747a013556a7f9` was read
for transaction commit/rollback, WAL sync/replay and FlakyCheckpointer failure
ordering. It is a mechanism reference only; no third-party source was copied
and no dependency was added. Accepted ZE-98/99/100/101/103/109 and the complete
current writes/identity/execution plans govern this implementation.

## Environment and exact commands

Host: Apple M3 Max, 128 GiB RAM (137438953472 bytes), 16 logical CPUs,
aarch64 macOS 27.0 build 26A5388g. Rust 1.93.0
(`254b59607 2026-01-19`), cargo-nextest 0.9.145
(`00af4550e 2026-09-16`). [Recorded commands/output](ze-37/host.json).
Fixtures are owned synthetic stores and exact scalar/canonical records;
there is no external dataset, model, credential or network dependency.

Every Rust command uses `CARGO_TARGET_DIR=target/ze37`. The committed nextest
configuration uses four processes, retries zero, captured output and one libtest
test per process. These are focused contract checks, not throughput measurements.
[Final gate manifest](ze-37/final-gates.json) records exact argv, workdir,
environment and exit codes. Console log views normalize trailing whitespace;
where that changes captured bytes, the adjacent `.log.gz` preserves the exact
original capture. Both forms have hashes in the evidence inventory. [Driver](ze-37/final-gates.py) retains the command
sequence. All eight terminal commands exited zero after mutation restoration:

| Gate | Actual result | Raw evidence |
| --- | --- | --- |
| Three public core suites | 55 passed: 27 staging, 17 key lifecycle, 11 canonical | [core](ze-37/terminal-core.log) |
| Staging allocation/framing library filter | 3 passed, 587 skipped | [library](ze-37/terminal-lib.log) |
| PG5 lifecycle and PG10 staging runner probes | 2 passed, 442 skipped | [runner](ze-37/terminal-runner.log) |
| Primitive staging oracle filter | 1 passed, 103 skipped | [oracle](ze-37/terminal-oracle.log) |
| Core lib + staging/lifecycle strict clippy | Passed with allocation-audit,test-support and `--no-deps -- -D warnings` | [clippy](ze-37/terminal-core-clippy.log) |
| Core rustdoc | Passed with `RUSTDOCFLAGS=-D warnings` | [doc](ze-37/terminal-doc.log) |
| Workspace formatting check | Passed | [fmt](ze-37/terminal-fmt.log) |
| Diff whitespace check | Passed | [diff](ze-37/terminal-diff.log) |

Additional strict oracle/runner lint passed before the final result-only
correction; those source files are unchanged in the final snapshot:
`cargo clippy -p zeppelin-embed-adversarial-oracle --lib --tests
-p zeppelin-embed-workspace-tests --test adversarial_tests --no-deps
-- -D warnings`. [Log](ze-37/clippy-oracle-runner.log).
`cargo deny check` passed advisories, bans, licenses and sources with existing
unused-allowlist warnings; [log](ze-37/deny.log). No Cargo manifest, lockfile or
deny-policy changes are included.

## Literal RED to GREEN

The archived RED logs contain the named failing assertion, not merely a test
that was written after the implementation. Paired historical GREEN logs show
the immediate correction; the terminal core/oracle runs repeat all final cases.

| Test or tested contract | Observed RED | Paired logs |
| --- | --- | --- |
| `invalid_endpoint_after_valid_node_exposes_no_ids_or_reservations` | Invalid later endpoint accepted | [RED](ze-37/endpoint-red.log), [GREEN](ze-37/endpoint-green.log) |
| `mixed_replay_preserves_original_generation_and_uniform_duplicates_reject` | Replay became new node 10/gen 8 instead of node 9/gen 5 | [RED](ze-37/replay-red.log), [GREEN](ze-37/replay-green.log) |
| `forward_local_endpoints_resolve_before_relationship_canonicalization` | Valid forward local reference rejected | [RED](ze-37/local-red.log), [GREEN](ze-37/local-green.log) |
| Receipt row limit, including retries | Zero-row cap accepted a receipt | [RED](ze-37/receipt-red.log), [GREEN](ze-37/receipt-green.log) |
| Lazy symbol creation and replay allocator exhaustion | Missing three changed symbol additions | [RED](ze-37/symbol-red.log), [GREEN](ze-37/symbol-green.log) |
| Pending property/text and deletion access | Pending change ignored | [RED](ze-37/overlay-red.log), [GREEN](ze-37/overlay-green.log) |
| Cypher final classification and consumed net-empty IDs | Finalization scaffold refused a valid batch | [RED](ze-37/cypher-red.log), [GREEN](ze-37/cypher-green.log) |
| All revision preconditions before private IDs | One Identity callback before later stale-item refusal | [RED](ze-37/preflight-red.log), [GREEN](ze-37/preflight-green.log) |
| Receipt cancellation seam | CoreResult checkpoint was missing | [RED](ze-37/receipt-checkpoint-red.log), [GREEN](ze-37/receipt-checkpoint-green.log) |
| Restrict before IDs; DETACH node tombstone | One Identity callback before Restrict rejection | [RED](ze-37/restrict-preflight-red.log), [GREEN](ze-37/restrict-preflight-green.log) |
| Cypher revision classification before generation | Generation overflow masked later revision failure | [RED](ze-37/cypher-classification-red.log), [GREEN](ze-37/cypher-classification-green.log) |
| Admitted high-waters cover retained incarnations | An incoherent base fence could alias an ID | [RED](ze-37/high-water-red.log), [GREEN](ze-37/high-water-green.log) |
| Fresh payload/aggregate framing and Restrict precedence | Fresh relationship duplicate properties followed 2 Identity calls; node interpretation followed 1; generation masked Restrict | [RED](ze-37/full-preflight-red.log), [GREEN](ze-37/full-preflight-green.log) |
| Structured/Cypher output preflight (four named tests) | Each valid create invoked Identity before rejecting output; at max generation GenerationOverflow masked Limit | [RED](ze-37/result-preflight-red.log), [GREEN](ze-37/result-preflight-green.log) |
| `result_registry_capacity_fits_writer_envelope_before_private_ids` | Known over-cap registry/writer overlap rejected after 1 Identity call | [RED](ze-37/registry-preflight-red.log), [GREEN](ze-37/registry-preflight-green.log) |
| Independent primitive oracle can fire | Stub oracle accepted erased participants | [RED](ze-37/oracle-red.log), [GREEN](ze-37/oracle-green.log) |

The consuming result-owner and Cypher result entry points also have missing-API
compile RED followed by implemented GREEN in `result-adoption-*` and
`cypher-results-*`; these are explicitly compile-contract RED, not behavioral
assertion evidence. Early fixture compile errors and an abandoned draft that
incorrectly charged registry bytes inside the ABI payload cap are excluded.
The authoritative separate ABI-cap proof is the mutation below.

## Can-fire, cancellation, allocation and independent model

[Mutation driver](ze-37/run-mutants.py) restores source in `finally`, requires
nextest exit 100 plus the intended assertion, and records before/mutant/restored
SHA-256. All five final controls fired and every original/restored hash matches
[the manifest](ze-37/mutation-results.json):

| Mutation | Detector |
| --- | --- |
| Remove both ABI arena-cap guards | Public complete ABI arena limit assertion |
| Skip final view check after materialization | Public ViewMismatch and release assertion |
| Skip shared accounting owner identity | Public foreign-owner refusal assertion |
| Rewrite an exact replay's original generation | Independent PG10 expected-versus-observed comparison |
| Remove allocation attribution wrapper | Scoped global allocation audit |

The terminal allocation audit measured **2,248 attributed bytes over 11
allocations**, **488 bytes retained**, **zero unattributed bytes**, **zero
allocations on denied admission**, plus a deliberately unattributed **17-byte
positive control**. This is one scoped synthetic fixture, not a universal
allocation or RSS claim. The production attribution mutant is separate proof
that the audit detects loss of attribution on the actual arena path.

The 200,000-byte text fixture encountered **92 checkpoints**; cancellation at
every cutoff returned rejection and released reservations, followed by a clean
paired completion. Long bytes poll in at most 64 KiB chunks. The DETACH fixture
represents 32,769 incident edges and observes zero incident probes plus one node
tombstone and both search-membership removals. Restrict exercises alive edges,
dead endpoints and staged explicit relationship deletion separately.

PG10's std-only primitive oracle predicts base classification, duplicates,
checked high-waters, per-item replay generations and changed participant lists
without importing engine types/codecs. The adapter uses actual public staging
and a real temporary Store accounting owner. Literal ZGCI-v1 scalar decoding
checks the original f64 bits independently. Seeds **0 and 41** each run 48 mixed
trials plus directed cases; each fires **3 cancellation/clean pairs**, **1
writer-budget/clean pair** and **1 oracle can-fire control**. All eleven required
coverage entries are wired into the existing runner. The focused probe ran;
the complete campaign did not.

Framing controls compare 60 provenance combinations against frozen v1 lengths
and actual encoding lengths/refusals, including cancellation and the 8 MiB limit.
Sizing and installed provenance use one encoder. Relationship endpoint zeroes
exist only in a length-counting sink; unresolved output refuses, and resolved
output matches exact full-width endpoint goldens. Existing canonical and key
lifecycle tests remain GREEN after the shared-rule refactors.

## Review, source inventory and remaining qualification

Root's first review found ordinary payload/framing and Restrict ordering gaps;
these produced the three named RED controls above before correction. Independent
v2 review verified all 25 frozen hashes and reviewed lifecycle/provenance
factoring, PreparedImage framing/resolved endpoints and consuming result drop
order. Its concrete output-layout ordering finding produced four additional RED
controls. The final [v3 correction review](ze-37/independent-v3-correction-review.md)
verified all 25 hashes and found no remaining concrete issue in that correction.
Review was read-only and ran no tests.

[Source inventory](ze-37/source-inventory.json) pins all 25 changed product,
test, runner and component-guide files, baseline hashes and complete-prefix
status. The property_graph export, canonical helper append, oracle export and
component guide preserve complete old prefixes. **Key lifecycle and provenance
contain real existing-body refactors**; retained old LLVM profiles are not proof
for their new mappings. The shared Accounting changes are authoritative ZE-49
source. Runner changes add the PG10 call and coverage entries only.

The user explicitly deferred broad nonessential plan-driven qualification to
**ZE-118 in E12 Backlog**, after implementation. No full workspace/adversarial
before/after campaign, per-crate 90% coverage result, final shipping graph size,
WAL/publication/recovery, interrupted fresh-store creation, real GraphStore
admission, C ABI registration or actual query-runtime adapter acceptance is
claimed here. Those obligations remain with ZE-118 or their owning integration
tickets. No graph WAL or VFS production path changed in this candidate.
The staged NoOp/Replayed result contains no changed generation or participants;
it is not yet proof of a future public coordinator emitting no WAL/checkpoint.
Main integration and ticket closure remain the coordinator's responsibility.
