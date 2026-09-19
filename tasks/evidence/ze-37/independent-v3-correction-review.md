# ZE-37 v3 correction-only review

Read-only review of frozen v3 against frozen v2; no builds, tests, product edits, or tracker writes.

Confirmed complete ResultLayout rows/core/ABI limits and checked core+ABI+registry+current writer capacity preflight run after actual receipt count/classification but before summarize_key_batch/generation/Identity on both structured and Cypher paths. The exact selected layout is retained for subsequent materialization without fabricated receipt identities or a second layout call. Actual allocations and capacity reconciliation remain later Prepare work. No remaining concrete finding in this correction.

Inventory verified: 25 files; mismatches: [].

- `crates/zeppelin-embed/src/property_graph/staging/overlay.rs`: `b0ffeb3a28a678006e74c179fdeeb35f79d1afaa75c0b2b769afa0bc8a405f3e`
- `crates/zeppelin-embed/src/property_graph/staging/result.rs`: `7b1a04487acf254ff0b8bcfd79b8dd1ffb7e15b21fd598fe2cc9744b46413de8`
- `crates/zeppelin-embed/src/property_graph/staging/structured.rs`: `a711b6c604a427743bd33ee71cdbf98996e54905bcf9ee3af8ea326f211e7a92`
- `crates/zeppelin-embed/tests/graph_write_staging.rs`: `e0a3f71bee850c4455903514c8833a625da9f6508552dd3cd15f7da5a9d0dff1`
