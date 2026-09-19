# ZE-128 independent frozen-source review

Disposition: no concrete correctness blocker found within the compiled internal aligned-response owner contract. This is not acceptance of the authentic graph producer, commit boundary, public C wrappers, or graph lifecycle integration.

Reviewer: `/root/ze73_fixtures`, delegated by root under the ZE-128 owner. Review was read-only with respect to repository source, tracker, index, and commits. No broad suite or new agent was used.

## Reviewed inventory

Owner confirmed all mutants restored and froze six source files before this review. All six files were copied byte-for-byte into `/tmp/ze-128-independent-snapshot`; their original inventory is copied there as `inventory.json`. The contract was copied as `owner-contract-proposal.md`. The original owner inventory is `/tmp/ze-128-evidence/frozen-source-inventory.json`.

| Source | SHA-256 |
| --- | --- |
| crates/zeppelin-embed-ffi/src/lib.rs | 9a00a4c5fdfe4daac324072fc53a2d3cff690541717049b9d90d18b60328730d |
| crates/zeppelin-embed-ffi/src/graph_result.rs | bdda71a432e6617dd5b0b3e7dddd056bac357e7481946a1ced8d20abbcf7252c |
| crates/zeppelin-embed-ffi/src/graph_result/audit.rs | 4fa5503389a933c8a6a39609c453bad236c505724193cd662564bd2970a66bef |
| crates/zeppelin-embed-ffi/src/graph_result/outcome.rs | 257b6b7e096776ad1ce0eb763fcd355e685b41062cf928048b0e257f0d9541eb |
| crates/zeppelin-embed-ffi/src/graph_result/registration.rs | c33f08564541130f0d2c75d340de1784f4e55a54ad38eb540eeedc8b2d83156f |
| crates/zeppelin-embed-ffi/src/graph_result/tests.rs | 1f6848b5723e4e1197a57f2ed8eb8101419d4b378a506c3dec7cd402983f5769 |

I read the live ZE-128 contract, proposal, all new implementation/test files, and the five-line feature-gated lib.rs export. Existing graph C descriptor definitions were checked for the zero-initialized canonical root and the compared immutable fields. No C export or header change appears in the candidate.

## Ownership and concurrency findings

- The fourteen pools use checked typed layouts, `Layout::extend`, final padding, and the actual retained allocation Layout for deallocation. Nonempty arrays are aligned independently; empty pools expose null and the zero-byte arena performs no allocation/deallocation. The 4 MiB bound applies after padding.
- Preparation has two actual fallible allocation sites: the arena and the stable registry node. Errors before node initialization release the arena; errors after node initialization are covered by OwnedNode; errors after list admission are covered by PreparedResponse abort. Admission's second capacity check closes the concurrent preparation gap.
- The embedded gate protects list links and removals with Acquire/Release. Normal operations try once and preserve ownership on Busy/Poisoned. Allocators, deallocators, and caller callbacks are outside the gate. Abort cleanup preserves poison, retries acquisition, unlinks, frees, and only then releases its query reservation. This cleanup can starve under scheduling and is correctly not advertised as wait-free/fair.
- Publication moves the accounting guard out and disarms abort before applying fixed successful metadata. It copies the returned descriptor before the Release publication store and performs no node dereference after that store. Free uses Acquire before reading the immutable published root. Thus the admitted concurrent free can destroy the node immediately after publication without a later node access in expose.
- Tokens are shared across registry instances and not reused in production. Private nodes cannot be freed even with an exact descriptor copy. Free matches all 42 root/pool scalar/pointer/count/range fields, checks caller-root overlap against authoritative arena and node extents, unlinks once, and releases allocations outside the gate. It does not use caller pointer values to discover allocation backing. Lookup remains O(configured outstanding results), distinct from two-allocation nonrecursive destruction.
- OutcomeCell preserves terminal known success and an Indeterminate attempt state without inventing IDs or generation authority. The caller still supplies authentic coordinator outcomes; the component does not establish that a commit occurred.

## Accounting and evidence findings

The implementation reserves actual padded arena bytes, actual Node size, declared preparation controls, and the external guard through the real RuntimeContext memory owner before allocating. Source ownership remains separately charged by the caller. Copy chunks charge CopiedBytes immediately before each bounded copy. CompletedAbiBytes is deliberately left to the existing coordinator/driver's single charge from represented_bytes; padding and registry controls remain separate capacity quantities.

I independently reran the four directed existing owner tests below against the worktree only after confirming all six files match the frozen copy. I checked the same six hashes again after the command; all stayed identical. These are independent reruns of the owner's tests, not newly authored independent test oracles.

```sh
ZE_TEST_SEED=128 cargo nextest run -p zeppelin-embed-ffi --features graph-cypher --lib -E 'test(graph_result_actual_allocator_failures) | test(graph_result_concurrent_free_publication) | test(graph_result_free_rejects_returned_root) | test(graph_result_seeded_cancel_sites)'
```

Result: 4 passed, 23 filtered out, exit 0. Evidence: `/tmp/ze-128-independent-directed.log` and `/tmp/ze-128-independent-directed.json` (command, seed, exit status, all before/after hashes).

The four reruns exercise actual allocation denial and heap balance, zero allocator attempts during expose/free, publication/free and abort concurrency, authoritative-root alias refusal, and all five deterministic cancellation sites with a clean same-seed control. The allocator audit is thread-local; I do not interpret it as a cross-thread or real commit-window allocator denial proof.

I also inspected the owner's terminal evidence: 13/13 component tests, 36 existing C/ABI tests with two explicitly named release-only skips, scoped clippy/fmt, and the three restored product mutants (arena deallocation, geometry comparison, final cancellation). The mutant record's restored hashes match the frozen implementation. Those prior runs are owner evidence, not additional independently rerun tests. The reported registration line coverage includes two cfg(test) controls; the README discloses this, and I make no production-only or whole-crate coverage claim from that number.

## Acceptance boundaries retained

ZE-68 must still provide authentic native producer ownership, valid semantic pool conversion, complete source/ABI/receipt/control overlap accounting, actual staging receipt adaptation, genuine commit-window allocator denial, and real coordinator outcome faults. This review does not credit C-shaped fixture slices as that producer. ZE-68/69 must choose and test public Busy/Poisoned behavior, descriptor retention, and the shipping outstanding-result bound; Poisoned is not a successful cleanup/recovery policy for already published results in this component. Actual graph close/reopen, public request marshalling/free ABI, header/artifact and packaging acceptance remain there. Broad qualification remains ZE-118.

No source change is requested by this bounded review. Root may evaluate the component candidate for integration while retaining the explicit downstream acceptance above.
