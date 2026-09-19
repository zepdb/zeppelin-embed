# ZE-129 primitive adjacency and liveness oracle interface

The model consumes already-normalized changed topology batches. It does not
classify public requests, revisions or replays and does not import engine types,
physical references, codecs, allocation cutoffs or publication machinery.

## State and operations

`Model::apply(generation: u64, operations: &[Operation]) -> Result<(), Error>`
atomically applies one nonempty batch at a generation greater than the current
generation. `Model::snapshot()` returns an owned immutable `Snapshot`.

`Operation` has the following variants:

- `CreateNode { id: u128 }`
- `DeleteNode { id: u128, detach: bool }`
- `CreateRelationship { rel: u128, source: u128, target: u128,
  relationship_type: u64 }`
- `DeleteRelationship { rel: u128 }`
- `PropertyOnly { entity_kind: EntityKind, id: u128 }`

Node and relationship identities retain all 128 bits and are never reused.
Duplicate entity targets in one normalized batch reject atomically. New
relationship endpoints may be nodes created by the same batch. An endpoint
deleted by the same batch is invalid.

Plain node deletion checks live incidents in the admitted pre-batch state after
excluding only relationships explicitly deleted by the same batch. A second
pending node deletion does not hide the far endpoint for this check. Therefore
deleting both nodes cannot authorize a surviving raw relationship, and every
live self-loop or parallel relationship must be explicitly deleted. An incident
relationship whose far endpoint was already dead is not live and does not block
plain deletion.

DETACH changes only node liveness. Raw relationship, OUT and IN rows remain.
An explicit relationship deletion can later remove such a retained raw row even
when an endpoint is dead. A property-only relationship edit requires both
endpoints live in the admitted pre-batch state and never changes topology. It
may coexist with same-batch DETACH: the raw property change is retained and the
relationship becomes invisible. Only a fresh relationship creation requires
its endpoints to remain live in the final batch state.

## Rows, queries and comparison

`RelationshipRow` fields and ordering are `(rel, source, target,
relationship_type)`. `AdjacencyRow` fields and ordering are `(bound_node,
relationship_type, rel, neighbor)`. A self-loop contributes one OUT row and one
IN row; parallel relationship identities remain separate.

`ObservationPlan` retains requested `RelationshipRange`, `AdjacencyRange` and
`DegreeQuery` values. Ranges are half-open and capacities apply after both
endpoint liveness filtering. The resulting `Observation` contains generation,
node liveness, raw relationships, raw OUT/IN, separately visible
relationships/OUT/IN, visible relationship count, degree results and every
planned limited-range result.

`Snapshot::check(&ObservationPlan, &Observation)` compares exact lengths, order
and scalar values. It never sorts, deduplicates, fills in or otherwise
normalizes an observed production result.

ZE-44 owns the production observer, actual producer and seeded runner binding.
This primitive model does not prove storage admission, physical adjacency,
durability, recovery or public traversal behavior.
