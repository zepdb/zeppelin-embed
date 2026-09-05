# 18. Carry one query deadline through queues and lexical execution

Status: implemented with52 focused passing cases and seven firing/restored
directed plants. Per-change host measurements are complete:48 ordinary API
processes,six native residual processes,and24 diagnostic attribution processes.
The required bounded-cancellation repair is retained with measured lexical
overhead disclosed in ASTRA-ISSUE-020;no ordinary-query speedup is claimed.
Full benchmark/suite qualification remains separate. This plan accompanies
the scoped implementation commit;Step19 is next and is the user-directed
stopping point. Steps20–33 will not start.

- Stage: Queueing and overlap.
- Execution class: Required.
- Prerequisites: [07](../07-pinned-result-materialization/plan.md).
- Intended commit subject: Carry text query cancellation through the full execution path.
- Shared requirements: [execution and validation contract](../README.md).

## Problem and intended result

TextStore::query_text creates fresh cancellation tokens internally. Embedding
queue waits are blocking, and the pruned lexical path checks cancellation only
around traversal. One expensive query can therefore occupy scarce workers after
its caller's useful deadline has passed.

## Implementation

Own text query options/entry points, runtime command admission and lexical
checkpoint seams. Preserve the existing query_text convenience method.

1. Add an additive controlled text-query method accepting the existing typed
   QueryControl model. One absolute deadline/token begins before admission and
   reaches embedding, retrieval, cross-fill and materialization.
2. Add bounded-frequency checkpoints to WAND/MaxScore, vocabulary expansion,
   DF construction and phrase work. Count unsuccessful seeks and metadata work,
   not only matches, when deciding to check.
3. Make queued work cancelable before execution. Poll/wake waits using existing
   monotonic clock semantics; never reset a relative timeout at each stage.
4. A running native model call may be non-preemptible. Check before and after it,
   suppress canceled output, report the actual bound, and do not claim immediate
   cancellation of foreign execution.
5. Keep all scoped jobs joined before borrowed data/leases are freed. Existing
   control/error precedence and partial=false behavior remain explicit.

## RED -> GREEN

Add astra_18_text_deadline_includes_embedding_queue_wait,
astra_18_pruned_lexical_cancels_inside_long_traversal,
astra_18_canceled_queued_embedding_is_not_executed,
astra_18_scoped_work_is_joined_before_cancel_returns and
astra_18_control_error_precedence_is_completion_order_independent.
Use a fake clock plus worker barriers. Include close, queue saturation, panic,
expired-on-entry and cancellation during native-call completion. Time-based
watchdogs can detect hangs but cannot establish the intended ordering.

## Measurements and acceptance

Measure cancellation latency in work units for preemptible loops and queue
drain/worker occupancy under abandoned requests. Record native-call residual
latency honestly. Verify a finite query cannot monopolize a lexical worker until
the whole postings list completes after cancellation.

## Validation and commit boundary

Exercise queue expiration, in-loop lexical cancellation, join lifetime and error precedence. Run the specific changed cancel/close/panic plants with controls; native residual cancellation is a separate measured host case.

Per commit: run this plan's named RED -> GREEN tests and only existing regression
cases that cover a changed contract not covered by those tests. The targets
below are entry points, not a requirement to run each complete test binary.
If a fault/ordering/error contract changes, run its directed adversarial probe
and same-seed clean control before/after; do not run a family smoke, campaign,
whole adversarial target or workspace suite for this commit.

Keep this commit within the implementation boundary above. Do not combine another numbered plan or unrelated cleanup.
Evidence: tasks/evidence/astra-18-query-cancellation.md. Use the shared checkpoint policy in ../README.md;
focused GREEN is not full-suite qualification.

## Focused test entry points

Use only targets that actually contain new or changed tests for this plan.
Filter to one named test for RED and identical GREEN; the plan prefix below
runs the small completed set. A command executing zero tests is not evidence.

    cargo test -p zeppelin-embed --test query_lifecycle astra_18_
    cargo test -p zeppelin-embed --test hybrid_bounded astra_18_
    cargo test -p zeppelin-embed-text --test query astra_18_
    cargo test -p zeppelin-embed-text --test adversarial astra_18_

Run a specifically named existing neighbor test only when it covers an
additional affected contract. Reuse passing evidence at the same relevant code
state; report native blockers without expanding into unrelated tests.
