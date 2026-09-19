# ZE-55 shared-resource correction review

Read-only frozen snapshot review; no builds, tests, worktree edits or tracker writes.

No concrete finding in the reviewed capacity seam. Frontend replacement Vec/String backing is reserved in full before allocation, including coexistence with old backing, and reconciles actual capacity. Vec moves poll per element; decoded-string copying polls per character; direct string copying is bounded by the parser input cap of 64 KiB. All retained frontend capacities remain conservatively charged until the scoped compiler invocation exits. Failed replacement/allocation/control drops private old/replacement backing before the outer anonymous reservation; the HRTB callback cannot return compiler borrows. Result lifetime and late cancellation drop copied callback results before releasing the frontend guard.

The new core QueryExternalReservation is only a grow-only anonymous reservation under the same QueryMemory/shared Accounting. It supplies no retained address proof or prepayment capability; later copied plan owners must hold their own reservations concurrently. Caller source/parameter backing remains explicitly borrowed and does not acquire a fabricated Vec-capacity credit. Hidden caller capacity and full graph admission are outside this adapter's claim.

Reviewed hashes:
- `crates/zeppelin-embed-cypher/src/resources.rs`: `15891cb2a504b472af958be2ed6b27817c51793c380028ece6ecaf517a9af254`
- `crates/zeppelin-embed-cypher/src/shared_resources.rs`: `20adf570b13355291a1afde073a0044b6dfdb5466a34971081457ce9ea7e3063`
- `crates/zeppelin-embed/src/property_graph/query/resources.rs`: `3c34d79e82e868ecfd38f40b9528ac052ff8fc5f916cc9e94b2e7892cabf3b64`

Also inspected frozen compile_with ownership flow and binding_resources tests for context; did not independently execute their assertions. Binder semantic validation and broader allocator qualification are excluded.
