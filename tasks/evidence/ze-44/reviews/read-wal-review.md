# ZE-44 independent reader and WAL binding review

Scoped PASS on the initial compiled producer snapshot, manifest SHA-256 a94b3fca95736709873e4efc149a92e9840a21d1bb98c5583fe3c5c7aa5c31cd, parent273eb33e66c281da18264d7f5eb7c08165e63ddf. All22 frozen source/log entries were verified before and after review. Production read.rs and prepare.rs remain byte-identical to that frozen snapshot after all deliberate mutations.

Root used an isolated parent archive plus16 exact owned files in /tmp/ze44-root-read-probe; no owner-worktree or main source edits. The retained-base/catalog fixture is explicitly synthetic, while staged batches, native producer, tree pages, records, both directional ranges, PreparedObjects and memory owner are real. This does not prove public GraphStore admission, a file/checksum lease, publication, recovery or durable reopen.

The existing actual producer test was extended only in scratch. Its new assertions check high-u128 half-open relationship scans and exact-type expansion, one-row capacity, plain-delete explicit-removal order, and actual corruption from missing endpoint payloads and physically removed node-directory entries across point lookup, scan, count, expansion, degree and incident detection. An unrelated self-loop remains readable.

A real staged DETACH writes a node tombstone without incident enumeration. Raw relationship/OUT/IN references remain unchanged; the formerly first edge is hidden before consuming the single output slot. Both scan and expansion return the later live self-loop; old retained roots still count3 edges and the new view counts1.

After finalizing actual produced objects, the review reconstructs their exact eight root references and object identities for a no-op participant fixture. No-op retains generation1/sequence101 and emits no private object. Eleven corruptions reject before any append: store, future generation, creation serial high-water, family, version, object size/geometry, artifact mismatch, original source generation, original source serial, swapped root slots and absent root slot. The metadata-only wrapper is not treated as checksum/file admission.

Command for every focused run:

```
cargo nextest run -p zeppelin-embed --test graph_storage_prepare authentic_native_graph_candidate_pairs_same_batch_self_and_parallel_edges -j 4
```

Baseline c4baedec PASS; initial probes4b14c561 PASS; complete visibility probes8b07ef33 PASS. Three isolated mutants each exited100 for its intended assertion: missing-node entry returns hidden, ignore far-endpoint liveness, bypass existing WAL metadata validation. Each source file was restored byte-for-byte. Terminal restored run6750d3ce PASS (one extended existing test, three other tests skipped). JSON and raw logs retain exact commands/results.

A scratch-injection preflight initially expected one insertion anchor, but the original test file contains three. It stopped before editing; a baseline test still ran and passed. The anchor was then limited to the first actual-producer test. This was tooling setup, not a product RED.

The owner independently found and is correcting equal-bound empty read intervals; that one-line post-freeze change is intentionally separate from this verdict and will receive its own follow-up evidence. Persisted descriptors remain strictly nonempty. Full history/consolidation thresholds, independent oracle/actual seeded runner, failure exhaustion and public lease acceptance remain on ZE44/45 and the remaining owning tickets. Broad campaigns remain ZE118.

## Exact empty-interval follow-up and owner handoff

The owner's one-line change permits equal bounds only in read.rs::check_range; all persisted descriptor validation is unchanged. Root reproduced the old-source RED100, then applied precisely <= to < and verified corrected read.rs SHA256 db4d6f4006daf95bbc0b11f203e96cd1e403634eee40e1fd91ef7f5d0591926c. All independent probes plus empty scan/expand and reversed-bound rejection passed (83c42020). The original owner regression was preserved verbatim in the handoff test; final combined handoff run passed.

The additive root-read-tests-on-empty-fix.patch is based on exact owner test df3692710f87913b2cd1070b8f1c9a3fbd38b544f957a7bebf5c06cc9aca34a3; git apply --check passed against the moving owner worktree when sent. It changes only the existing actual producer test, not production. Source mutation controls and reports above remain tied to the original frozen hash; this paragraph names the exact follow-up separately.
