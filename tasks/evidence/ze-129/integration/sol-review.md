# ZE-129 independent primitive adjacency/liveness oracle review

Verdict: **PASS; no concrete blocker or correction found** in candidate commit
`732cb3d3716e0b74b498b9eec9bcd01d7d341adc` against base
`0e0064ddb3b1630e8c18050cb78c30ac7bcfb638`.

This was a read-only review. No repository, worktree, tracker, comparator, or
legacy oracle file was edited, and no broad suite was run.

## Frozen input and preservation

The owner files were copied before review into
`/tmp/ze-129-sol-review.90XCrN/`. Their SHA-256 values matched the owner
worktree before and after the candidate commit:

| File | SHA-256 |
| --- | --- |
| `tests/adversarial-oracle/src/graph_adjacency_store.rs` | `371cba222a652aa1e3c2661134b1dbcfa220cbc696c1ee84d8d1924ed0feb067` |
| `tests/adversarial-oracle/tests/graph_adjacency_store.rs` | `a7d93f181bdbb08c5665861951edee34d109a425134257a2972fd8dbd02e4345` |
| `tests/adversarial-oracle/src/lib.rs` | `e1ca89fd1d15988da4070819b59c854701dc078020fa47ee82c0210ca5fd0c20` |
| unchanged `tests/adversarial-oracle/src/graph_adjacency.rs` | `c8e97624db4f0af90d0e2b0f91c62d402ee49095d38f994e641f9cb8ab748868` |

The committed `tasks/evidence/ze-129/source-hashes.json` contains those same
values. `git show --check 732cb3d` passed. The commit contains exactly the
seven declared owned paths: the two model/test files, additive `lib.rs` export,
and four `tasks/evidence/ze-129/` files.

## Standards

PASS; no actionable standards finding.

- The model imports only `std`; the oracle crate manifest has no dependencies.
- `Model::apply` validates the complete batch, mutates a cloned candidate, and
  installs it only once after validation. Rejected batches cannot partly alter
  model state.
- The new comparator compares observed lengths, order, query values, and scalar
  values directly. Its only sorts build the independent expected raw OUT/IN
  streams; it never sorts, deduplicates, fills, or repairs observed input.
- The new source contains no engine type, codec, classifier, physical reference,
  production helper, unsafe code, or panic site.
- The older `graph_adjacency::check_paired` still sorts copied observations, but
  its source hash is unchanged and the new model neither calls nor shares code
  with it. That remains a separate legacy pure-codec oracle boundary.

## Spec

PASS; no missing, incorrect, or scope-creeping behavior found.

- `apply` models only nonempty, already-normalized changed batches at increasing
  generations. It retains permanent full-width node and relationship ID sets,
  rejects reuse, admits same-batch new endpoints, and requires fresh-edge
  endpoints to be live in the final batch state.
- Relationship `PropertyOnly` checks visibility in the admitted pre-batch state,
  so it may coexist with same-batch DETACH. DETACH changes node liveness only;
  raw relationship/OUT/IN rows remain, and a later raw `DeleteRelationship`
  remains legal.
- Plain DELETE checks pre-batch live incidents and excludes only relationship
  IDs explicitly deleted in that batch. A pending deletion of the far endpoint
  does not hide a live incident; an already-dead far endpoint makes the retained
  raw edge non-live and therefore non-blocking.
- Raw authoritative relationships are ordered by full RelId. Raw OUT/IN use
  `(bound_node, relationship_type, full RelId, neighbor)` order. Visible
  relationships, both directions, degrees, counts, and limited ranges all
  filter both endpoints before capacity.
- Snapshots own cloned state. Half-open finite ranges, `None` infinity, empty
  ranges, zero capacity, and `u128::MAX` are handled without wraparound.
- The tests include the required same-batch, self/parallel, high-degree DETACH,
  full-ID, range, capacity-mask, ignored-delete, missing-reverse, topology/type,
  all-dead-data, and old-snapshot controls.

Residual focused-test limitation: permanent ID reuse and duplicate-target
rejection are implemented but do not have dedicated named tests in the frozen
eight-test file. Static inspection found the checks, and the independent probe
below exercised node and relationship non-reuse. This is not an observed
contract defect.

## Independent directed check

I compiled and ran `/tmp/ze-129-sol-review.90XCrN/probe.rs` directly against the
frozen model:

```text
rustc --edition 2024 -Awarnings probe.rs -o probe && ./probe
exit 0
```

The probe checked atomic rejection of two pending plain node deletes with a
surviving live relationship; acceptance when that relationship is explicitly
deleted; permanent node and relationship ID non-reuse; relationship
`PropertyOnly` with same-batch DETACH; raw-row retention after DETACH; and later
raw relationship deletion. It then re-hashed the four frozen/owner files; every
hash still matched the table above.

The owner's retained evidence reports focused nextest **8/8 GREEN**, strict
all-targets oracle clippy GREEN, rustfmt GREEN, dependency inspection with no
dependencies, and diff check GREEN. I inspected those records but deliberately
did not rerun that redundant focused suite.

## Qualification boundary

This pass covers the std-only primitive expected-value model and exact
observation comparator only. It does not establish identity with a runtime
model, nor accept ZE-44's actual producer/observer, real native OUT/IN and
authoritative-directory agreement, admission, splits/consolidation,
publication, failure handling, durability/recovery, seeded can-fire controls,
or public traversal. ZE-44 retains those obligations; broad qualification
remains ZE-118. The runtime did not independently expose a model identity, so
this report makes no such claim.

Finding summary: Standards 0; Spec 0. Worst issue in either axis: none.
