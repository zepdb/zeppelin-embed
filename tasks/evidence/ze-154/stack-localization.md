# ZE-154 stack localization and narrow next correction

Read-only Astra diagnosis, 2026-09-20. No product edits, builds, test runs, or tracker mutation by this reviewer. Sol remains executor. The root-approved arena ownership correction remains insufficient: run 8f85f807 still overflowed. Do not claim GREEN or change stack limits, budgets, fixtures, or old operator semantics.

## Located failure

An existing macOS crash report already answers the proposed constructor-versus-pull diagnostic:

`/Users/aghatage/Library/Logs/DiagnosticReports/zeppelin_embed-cf262d81751aec60-2026-09-20-134617.ips`

Capture time is 2026-09-20 13:46:12.9089 -0700, following the state-ownership correction. The faulting thread is the unchanged native_pattern_keys_labels_liveness_full_ids test. Its stack contains:

```
KeyPatternConsumer::consume -> execute_in -> drain -> NativePattern::pull
 -> Collect next_occurrence
 -> LookupKey next_occurrence -> next_key
 -> LookupKey next_occurrence -> next_key
 -> LookupRelationship next_occurrence
 -> LookupNode next_occurrence
 -> GraphReadView::lookup_node -> lookup_node_state -> verify_node_state
 -> verify_record -> verify_canonical -> NativeCatalog::resolve
 -> payload UTF-8/span resolution -> NativeQuerySource::resolve/decode/check_required
 -> NativeGraphBundle::required_object -> iterator chain find -> stack overflow
```

There are five live next_occurrence dispatch closures. The Unit child returned before node verification. Construction completed. No relational operator executes in this consumer. The lookup is authentic and the terminal iterator frame is where the remaining stack runs out; this is not evidence of infinite recursion in required_object.

Sol also completed root's already-approved one-marker diagnostic b28ad27e-b4a5-4d1a-bc11-8182309e26c9, which printed `ZE154_STACK_PHASE entered_key_test` and aborted at 0.128s. Sol reports the marker was immediately reverted; raw output is `/tmp/ze-154-stack-phase.log`. No further localization run is needed.

## Compiled frame evidence

Read-only LLDB disassembly of the already-built test binary, without launching it:

| Frame | Current post-arena binary | Existing root binary |
|---|---:|---:|
| next_occurrence | 0x20 + 0x1000 + 0x9f0 = 6,672 bytes | same |
| next_occurrence dispatch closure | 0x60 + 0x6000 + 0xdf0 = 28,240 bytes | 0x60 + 0x6000 + 0x780 = 26,592 bytes |

Thus the common recursive dispatch closure is 1,648 bytes larger in the current binary; five live frames consume 8,240 additional bytes. Shrinking PhysicalState restored the outer frame size, but did not remove new dispatch temporaries. The unchanged test body itself reserves approximately 1.25 MB; it must remain unchanged under the task contract.

The existing root binary is a useful static comparison, **not an independently rebuilt/pinned baseline acceptance run**. Its debug locations are pattern.rs:312/328, matching the pre-relational source layout. The current inspected binary was Sol's one-marker binary; the marker affects only the test body, not these production functions. Full commands, binary SHA-256 hashes, raw prologues and crash-stack extraction are retained in `/tmp/ze-154-stack-localization-static.txt`.

## Smallest recommended next correction

Source-local evidence identifies residual stack growth in the common recursive dispatch, not in construction. Its new Sort/Distinct/Aggregate arms each execute a checked `as_mut_slice().first_mut().ok_or(RuntimeError::Batch)?` before calling the dedicated helper. Those accesses create additional fallible-expression temporaries in **every** dispatch closure, including this old six-operator consumer.

Move exactly those three checked one-element accesses into the corresponding existing `next_sort`, `next_distinct`, and `next_aggregate` helpers in `pattern/relational.rs`:

- Change each helper's state parameter from `&mut XxxState` to `&mut QueryArena<'m, 'g, XxxState<'v, 'm, 'g>>`.
- At the beginning of each helper, bind `let state = state.as_mut_slice().first_mut().ok_or(RuntimeError::Batch)?;`.
- Change each common dispatch arm to a direct `self.next_xxx(index, *child, state, context)` call.
- Keep all algorithms, allocation/charging, OffsetLimit, reset, inheritance, state extraction/restoration and old operators untouched. In particular, an error returned by a helper still flows through the existing closure result and PhysicalState restoration.

This is a focused next correction with a falsifiable prediction, not yet a verified fix: isolating these new fallible temporaries from the common recursive dispatcher should reduce its frame and allow the unchanged regression to finish. It introduces no new abstraction, allocation, unsafe operation, test, stack setting, or exception. Root should approve this observed-evidence adjustment before Sol changes production code.

Run only the unchanged isolated regression first, with the exact ordinary settings already used:

```
cargo nextest run -p zeppelin-embed --lib --features graph-cypher -j 4 --retries 0 -E 'test(native_pattern_keys_labels_liveness_full_ids)'
```

If GREEN, continue the already-required reset group and finite final selection. If still RED, stop further changes and compare that binary's common dispatch prologue against the recorded values; do not generalize the executor, extract unrelated old operators, or fix the last storage iterator just because it appears at the crash tip. No broad/full/adversarial, fuzz, coverage, soak, or release execution is added.
