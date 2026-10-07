# Crafted pre-ZE-380 pending reclaim proof

The release v0.6.0 fixtures are vector-only and contain no pending graph proof.
This image was crafted with the unchanged old producer at starting HEAD
`56454c5626e031e00725e391cb40cb1cf4910426` in a `git archive` checkout.
It contains one keyed text node (`reclaim-proof/legacy`), one replacement,
a checkpoint, and a durable pending reclaim intent. The protected stream
contains both legacy tag 3 WalAuthority and tag 4 CapturedState; the generator
walked the stream and asserted both before copying the directory.

The old reader opened a copy read-write and resumed the intent successfully;
the pending image was then restored from the untouched source. No persisted
identity was changed or derived from the fixture location. The new reader's
`an_old_pending_reclaim_proof_is_refused_without_mutation` copies this directory,
checks both access modes, requires a specific legacy-tag error, and compares
all file bytes plus the absence of Delete events. Finish the pending cycle
with the previous binary before upgrading; this reader deliberately refuses
old proof authority instead of translating deletion permission.

Generation command (CARGO_BUILD_JOBS=3):

```
ZE380_LEGACY_FIXTURE=<fixture-directory> CARGO_TARGET_DIR=/private/tmp/ze380-baseline-target cargo test --manifest-path /private/tmp/ze380-baseline/Cargo.toml -p zeppelin-embed --lib --features graph-cypher lifecycle::native_graph::tests::consolidation::ze380_generate_legacy_pending_fixture -- --exact --nocapture
```

Generator source and raw result are retained in `/private/tmp/ze380-baseline`
and `/private/tmp/ze380-logs/legacy-fixture-generation.log` for this session.
