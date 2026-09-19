# ZE-67 schema review

Read all1330 lines of frozen /tmp/ze-67-schema-review/graph_contracts.rs, SHA070e95ed36869fedb86a43a42ef2231f10614c022fc8a3d8333536b8626e82c2, against accepted bindings/execution/cypher/identity plans and main domain/query code. No builds or tests run by this review.

Required corrections before freeze:
1. Empty graph names, namespace/key and relationship-type names are legal exact UTF-8, including NUL (main property_graph/names.rs). Remove nonempty restrictions on these fields and add positive shape controls. Filesystem path retains its distinct nonempty/no-NUL rule.
2. Represent explicit caller-tightened query/work limits in typed C options, matching execution.md21 and actual QueryMemory/WorkLimits. Optional limit presence must not silently turn a zero allowance into defaults. Requested windows must retain core SearchBounds zero-as-tightened-budget distinction. Later69 owns full marshalling/semantics; no runtime stubs required in67.

Already incorporated prior-review requirements in this snapshot: explicit omitted/Auto/Exact/Scan/Graph tier identity, independent hybrid component slots, same-view eligible-set producer/slot, OR relationship types and bounded per-edge predicate, document-only persisted open tower, separate query/alignment interpretation, and payload field selection confined to typed entity-get (queries retain StoredText expression).

Proposed strong IDs/handle, separate generated graph contract header, default-disabled unsupported-platform feature, append-only errors35..54, fixed-stride versioned structs, value/list/disposition tags and pure-shape validation boundaries fit the accepted direction. Exact physical/tag goldens, output shape rules and later runtime integration still require their own evidence. No final contract acceptance claimed until corrections and candidate review.
