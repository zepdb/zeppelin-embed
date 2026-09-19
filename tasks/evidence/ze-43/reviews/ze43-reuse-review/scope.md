# ZE-43 admitted private-reference reuse correction

Frozen source for bounded ownership/performance review. Baseline is the accepted 38-file PG8 snapshot /tmp/ze43-pg8-review plus accepted /tmp/ze43-final-review-delta comment/lint changes. See changes-from-pg8.patch and hashes.json. Production behavioral delta is exactly artifact/private.rs, payload.rs and stream.rs. prepared.rs is the already-reviewed identity-only comment change.

Private VerifiedEntry is constructed only after complete append-time header/hash validation; exact full PhysicalRef plus immutable same-owner backing permits repeated resolution without rehashing. Failed state cannot resolve even an earlier good entry. Full seal decode and fresh-file admission remain unchanged. Internal bounded payload spans charge only the actual visible window, while public full-span behavior is unchanged. Copied/compared/UTF-8/scalar bytes retain explicit bounded work charging. No limits changed.

Named RED tests and green log: /tmp/ze-43-evidence/{red-bounded-admitted-reads.log,green-bounded-admitted-reads.log}. PG8 route RED then GREEN: red-pg8-private-reuse.log and pg8-after-reuse.log. Full focused storage 58/58 in storage-final-after-reuse.log. Strict storage and PG8 lint in storage-lint-after-reuse.log and pg8-lint-after-reuse.log.

Original 2048 labels/properties test now uses original 200M budget, 54,075,056 work units, 71,705 canonical bytes, 65,680 native bytes and 879,696 participant peak bytes. Prior diagnostics and failure are preserved in index-diagnostic-{128,256,512}.log and large-native-index.log. Native index sorting/semantic comparison remains O(n log n); removed repeated O(payload bytes) private block hashing and unrelated payload tail charging.

The new private exact-reference test reports zero allocations/attribution during ten repeated accesses, checks changed length/kind/version/offset refusal and reuse after append/seal. Existing failed append/seal/finish test now explicitly rejects prior-good-reference resolution after failed append validation. PG8 required registry key property-graph.directories.private-reuse exercises actual PreparedObjects/PrivateArtifact each generation, measured work and unchanged capacity, and altered kind/length refusal.

Parser target is included only for source correspondence, not requested independent broad parser review. Original harness classification issue, fixed-input replay, and prior bounded 60s run are saved separately. No broad campaigns or GraphStore/writes publication claim.
