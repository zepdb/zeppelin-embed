# ZE-61 sparse retrieval participant evidence

ZE-61 adds the private sparse text/vector retrieval participant to the admitted
native graph preparation path. This evidence is scoped to the component
contract. Public durable reopen/publication, recovery admission, ranking/ANN,
and GC retirement remain owned by their follow-up tickets.

## RED

The first valid RED used the real
`stage_structured -> NativePreparationSource -> GraphPreparation` coordinator
fixture. The finished `PreparedObjects` container had no role-6 retrieval
participant. The earlier assertion against `expected_text()`/`expected_vector()`
was discarded because those getters describe the admitted base, not prepared
output.

Subsequent behavior REDs included:

- replacement removal left a prior live membership bit set;
- a semantically corrupt full-identity/source-row participant passed outer
  framing before the correlation checks were added;
- required sparse byte mutations were not all rejected;
- a foreign native/sparse preparation pair was accepted;
- bounded trace omitted required descendants;
- query-side sparse decode did not preserve the admitted runtime owner/resource
  boundary.

## GREEN boundaries

The final fixtures exercise actual persisted artifact bytes and authentic
admitted leases:

- independent exact text/vector `(full NodeId, revision)` sets, including two
  low-64-colliding IDs, source-local dense ordinals, analyzed-empty exclusion,
  all-zero vectors, and exact original `f32` bits;
- private COW transitions T-only -> V-only -> both -> neither, analyzed-empty
  replacement, delete/detach-delete, unchanged unrelated membership, a
  relationship delta with no sparse source or membership, and replay/NoOp
  classification with empty deltas;
- a real mid-preparation cancellation after partial pack creation, with every
  partial object in abort inventory, the original lease/base retained, and
  charged storage returned to baseline;
- exact row/native identity, revision, ordinal, source, canonical record,
  provenance, generation and interpretation correlation, plus framed semantic
  missing-node/tombstone negatives;
- checked checkpoint preparation and replay-transition validation over actual
  packs, exact cutoff markers, canonical/provenance origins, active memberships,
  and untouched full membership values;
- mandatory source-bound native+sparse handoff before packs finish and rejection
  of a foreign participant from the same numeric generation/sequence;
- complete bounded trace with capacities 1, 2 and 256, all directory pages,
  immutable source rows (including retained dead rows), payload descendants,
  a real extent list from a canonical payload larger than 64 KiB, and latched
  missing/corrupt child failure;
- authentic `GraphReadView` sparse lookup and source iteration, same-memory but
  different-runtime refusal, direct foreign-owner refusal, memory/work limits,
  in-loop cancellation/deadline, close cancellation, and generation-zero empty
  state.

The existing producer migration was checked by the focused coordinator and
retained-view tests: coordinator run `ab4f3be7-2851-4ec7-82bc-3c8d04ed1028`,
retained every-edge run `7545223d-1893-4414-a53c-79366c3e0b04`, and complete
handoff/relationship successor run
`1cb3d124-5f47-4a06-8487-e0e34ffdff6b`.

## Final focused acceptance log

Command:

```text
cargo nextest run -p zeppelin-embed --lib --features graph-cypher -j 4 --retries 0 -E 'test(ze61_sparse_populations_match_model) | test(ze61_replace_delete_are_atomic_private_participants) | test(ze61_full_identity_revision_and_row_correlations_are_checked) | test(ze61_checkpoint_and_replay_match_active_model) | test(ze61_required_sparse_bytes_fail_loudly) | test(ze61_complete_search_handoff_precedes_pack_finish) | test(ze61_search_trace_is_complete_and_bounded) | test(ze61_sparse_resources_use_the_admitted_owner)'
```

Result: nextest run `be42585f-3302-4503-bbe3-71dd96e31b78`, 8 passed,
620 skipped, retries disabled.

## Feature compile controls

All five required narrow controls exited 0 on 2026-09-20:

```text
cargo check -p zeppelin-embed --lib
cargo check -p zeppelin-embed --lib --features graph-cypher
cargo check -p zeppelin-embed --lib --features graph-cypher,test-support
cargo check -p zeppelin-embed --lib --features graph-cypher,allocation-audit,query-timing
cargo check -p zeppelin-embed --lib --features allocation-audit,query-timing
```

The commands retained the already-recorded unused ZE-146 legacy helper
warnings; no warning cleanup was performed.
