# ZE-121 primitive directory oracle interface

This models normalized logical storage transitions from independently authored
fixture operations. It does not call the engine, its lifecycle classifier,
canonical codecs, tree ordering helpers, or other expected-value modules.

- `Entity { kind: Node | Relationship, id: u128 }`.
- `Key { kind, namespace: Vec<u8>, key: Vec<u8> }`; UTF-8 validated, exact bytes,
  empty and NUL allowed, ordering `(kind, namespace bytes, key bytes)`.
- `Image { canonical: Vec<u8>, shape }`; shape is `Node { labels: Vec<u64> }`
  in sorted unique numeric order or `Relationship { source: u128, target: u128,
  rel_type: u64 }`. Canonical bytes are supplied by the independent fixture,
  never encoded/decoded or inferred from physical locations by this model.
- `Operation { kind: Create | Put | Delete | Recreate | Cypher, key: Option<Key>,
  expected: Absent | Entity(Entity) | Deletion(u64), incarnation: Entity,
  revision: u64, delete_mode: Option<Restrict | Detach>, image: Option<Image> }`.
- `Provenance` is independently derived with supported version 1, every request
  field above, requested/installed revisions, incarnation and the changed
  generation. `Record` carries the image and provenance; `Fence` carries the
  provenance and optional live canonical bytes. No checksum substitutes for
  byte equality.
- `Model::new(Limits)`; `apply(generation, Operation) -> Result<Outcome, Error>`
  performs one normalized transition. Changed operations must use a newer
  generation. Exact structured replays retain the original generation and
  current root; rejected operations leave it unchanged. This is a sequential
  primitive trace, not a public batch-admission or fault/publication model.
- `snapshot()` returns an independently owned immutable snapshot. `relocate`
  advances only the view generation, preserving every logical record/fence.
- Snapshot lookup and bounded half-open range APIs cover full numeric IDs,
  exact keys, label membership and relationship-type membership. The complete
  `Observation` has sorted raw nodes, raw relationship records, fences,
  `(label,id)`/`(type,id)` entries and complete retained node-tombstone
  deletion provenance (including unkeyed Cypher deletions).
- `snapshot.check(&Observation)` compares exact order and all contents; it
  never sorts/deduplicates production observations. Stable difference paths
  identify canonical/provenance/member/old-view errors.

ZE-43 confirmed the ZE-109 distinction: DETACH updates only its node and keeps
incident raw relationship records and raw type entries. A separate observable
relationship query filters *both* endpoint liveness. Node tombstones remain until
all stored incident relationships have been removed; an explicit maintenance
operation may then drop the tombstone, while used IDs remain unavailable forever.

Limits bound transitions, ever allocated identities, key fences, bytes in each
canonical image/key and labels per node. Each snapshot and range result is
bounded by those limits. Snapshot retention by the caller remains test-harness
ownership, not a claimed engine allocation budget. Object inventory and physical
references are absent; neither can manufacture logical entity liveness.

The production source/sink and scheduled-fault PG8 adapter remain ZE-43. These
primitive tests do not prove page traversal, reopen, fault firing or durability.

## Confirmed storage-owner boundary

The ZE-43 owner reviewed this interface before implementation. Physical ledger
ordering uses numeric NamespaceId followed by exact key bytes. This oracle's
logical key ranges use known namespace bytes; the adapter must explicitly map
the known catalog fixture and separately verify physical range ordering. The
oracle never silently reorders a supplied Observation.

Frozen implementation entry points are `Model::{new,apply,snapshot,relocate,
drop_node_tombstone}` and `Snapshot::{generation,lookup,fence,node_range,
relationship_range,key_range,label_members,type_members,
observable_relationships,observation,check}`. Both numeric range ends and key
range ends are exclusive; `None` is unbounded, including maximum u128 IDs.

`Operation` describes one independently authored normalized fixture request.
The model requires already-resolved endpoint IDs, sorted unique numeric labels,
and independently supplied logical bytes; it does not run a canonical codec or
reconstruct a public request from production observations. A Cypher operation
that changes its final image must provide the next checked revision. A genuine
unchanged image is NoOp before revision increment, including at u64::MAX.
Generation advancement belongs only to changes and explicit maintenance.

This sequential model deliberately does not predict mixed-batch admission,
allocator high-waters, automatic incident-edge sweep, corruption classifications
or complete GraphStore/Cypher public request validation. The production adapter
must supply explicit normalized operations and independently inspect its native
roots. Those boundaries remain ZE-43 and the respective later component owners.
