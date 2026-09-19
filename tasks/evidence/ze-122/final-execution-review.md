# ZE-122 final execution and bindings contract review

2026-09-19. Read-only review of
`tasks/evidence/ze-122/parallel-contracts.md`, SHA-256
`9292258628b7ef17bd0f956c76c9ca9942bd5923e68b01be48fbd0826195181c`,
the corresponding plan document, and live ZE-123 through ZE-128 descriptions.
The checked ticket-edge snapshot is
`final-execution-review-snapshot.json` in this directory. No product or tracker
changes and no tests performed by this review.

## Content review

No concrete execution/C contract content blocker found. The document and new
tickets faithfully preserve the earlier audit:

- Landed QueryValue/QueryMemory/QueryArena/RuntimeContext/RowBatch interfaces are
  distinguished from proposed native result, registry and operator composition
  APIs. ZE-125 explicitly compiles a multi-batch composed producer first;
  ZE-127 owns its compiled native result/transfer API before conversion;
  ZE-128 owns real aligned C storage and registry admission before adapters.
- Actual capacities, padding, simultaneous buffers, authentic guard transfer,
  query/writer/shared bounds, cancellation and cumulative counters remain
  required. Existing Vec<u8> and HashMap insertion are not misrepresented as an
  aligned arena or allocation-free registration contract.
- Final application-owned results cannot retain query/store/view/caller
  lifetimes. Native-to-C conversion, real preparation/coordinator outcomes,
  commit-window allocation denial and close/reopen remain mandatory in ZE-68.
  Public C entrypoints remain ZE-69; no reverse dependency cycle is proposed.
- ZE-125's kernels and real eligible-set owner are useful production code
  independently of native scans. ZE-127's owned values and ZE-128's actual
  allocator/registry are useful independently of GraphStore execution. Their
  fixture/adapter tests are explicitly limited and cannot close the original
  public-path or lifecycle criteria.
- Original ZE-50/51/52/53/68 acceptance and the ZE-109 DETACH correction are
  retained, including exact bags, real compaction/reopen, admission, shared
  ownership, per-call reports, no partial results and actual outcome faults.

The plan document's differing relative reference to the evidence directory is
appropriate; byte identity with the evidence copy is not required.

## One blocking final-state finding

At the inspected live snapshot, the required original-ticket edges have not
yet been installed. ZE-124/125/126/127/128 each reports `blocks: []`; ZE-123
blocks only ZE-126. Original ZE-44/50/51/52/53/56/68 still have their old
dependency lists. The document states these new gates already exist and that
ZE-78 includes all six, while `tasks/evidence/ze-122/dependency-audit.json` is
not yet present. This may be root's ongoing final update; it is a final-state
gate, not an objection to the proposed dependency design.

Before closing ZE-122, install and verify these native edges:

```text
ZE-44 blocked by ZE-124
ZE-50 blocked by ZE-123 and ZE-46
ZE-51 blocked by ZE-125
ZE-52 blocked by ZE-40
ZE-53 blocked by ZE-127
ZE-56 blocked by ZE-126 and ZE-53
ZE-68 blocked by ZE-128
```

Retain the already-created ZE-126 blocked by ZE-123 edge and all original
dependencies. Persist the before/after closure/cycle audit, original criteria
mapping, affected epic/spec addenda and backup as promised. The review is
content-approved subject to that concrete state being verified; it does not
authorize claiming the extra dependencies are installed before they are.
