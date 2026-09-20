# ZE-141 Spec Review

Fixed diff and sole commit confirmed. Four acceptance gaps:

- **HIGH — checkpoint/resource-failure coverage is substantially partial.** The plan requires sweeping every checkpoint, >64 KiB byte *and descriptor* outputs, exact copied-byte prefixes, native/C work-limit seams, each final Completed-byte charge, deadline, and final close/cancel cleanup (`/tmp/ze-141-astra-conversion-plan.md:185`). The named test only audits allocator ordinals, both C allocation hooks, and the final checkpoint for an empty response (`crates/zeppelin-embed-ffi/src/graph_result/conversion/tests.rs:1423`, `:1473`, `:1502`). Those omitted failure paths and cleanup guarantees remain unproved.

- **HIGH — PG16 omits required source/native memory refusals.** Four seeds must cover “genuine source/native/C memory refusal” with paired clean controls (`/tmp/ze-141-astra-conversion-plan.md:210`). The probe covers two C allocation refusals, cancellation, a native copied-work refusal, and one C overlap-memory refusal (`tests/adversarial/graph_response.rs:540`, `:573`, `:607`, `:630`); it has no source-owner or native-copy memory-refusal pair.

- **MEDIUM — typed rejection/context evidence is incomplete.** The plan requires all native source errors plus malformed ranges, list cycles, outcome/report contradictions (`/tmp/ze-141-astra-conversion-plan.md:177`), and close-over-cancel precedence/prior-work carry-through (`:179`). The tests cover UTF-8, ForeignView, and one-call snapshot stability only (`crates/zeppelin-embed-ffi/src/graph_result/conversion/tests.rs:939`, `:977`, `:1001`). `Missing`, `Deleted`, `Storage`, the remaining malformed shapes, precedence, and prior-work assertions are absent.

- **MEDIUM — mandatory copied-byte mutation proves only half its contract.** The matrix requires removing a C chunk charge to fail both known-byte-delta and C-work-refusal assertions (`/tmp/ze-141-astra-conversion-plan.md:225`). Mutation 09 runs only the final-counter test (`tasks/evidence/ze-141/run-mutations.py:173`), so no C-stage work-refusal assertion is killed.

No scope creep or confirmed production-mapping defect was found. The ZE-53/68/69, TCK, public ABI, coordinator, and broad-qualification exclusions are stated honestly.
