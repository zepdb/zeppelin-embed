# Client parity v1 — ZE-386 / ZE-367

synthetic corpus generated from ADR-017 assumptions, NOT the client's recorded queries: replace or extend with the client's real corpus when provided.

The generator is `bindings/node/bench/client-parity-generator.mjs`, beside the existing Node query-filter benchmark. No dependencies were added. Seed 386; stable xorshift32; IDs are ordinal u128-compatible integers, independent of filesystem paths.

ADR-017 specifies 500 meetings/year, 300 transcript segments/meeting, about 30 words/segment, 1,000 additional notes/summaries, and 4–6 attributes. We generate 151,000 documents for one year. Transcript lengths are paired 20–40 words, averaging exactly 30; two 60-word note/summary documents follow each meeting. `documentBatches(5)` produces 755,000 documents including notes/summaries (750,000 transcript segments). Parity uses one year, not a reduced sample.

ADR-017 does NOT specify a vocabulary, length distribution, folders, query mix, or vector dimension. Synthetic choices: 24 meeting words, 10 topics, 64 prefix-expansion terms, emoji/accented/CJK text, 10 folders, 4 speakers, two streams, six u64 attributes (note, folder, speaker, stream, start/end offsets), meeting timestamp, and 8-dimensional seeded float32 vectors. This is a correctness corpus, not representative embedding quality or ADR-017 performance acceptance.

`queries.json` contains 500 requests, 100 each: Home lexical search with 300-byte snippets; type-ahead with lastAsPrefix; folder IN filters, half also constrained to 302 eligible document IDs; chat hybrid with exact vector tier and alpha 0/0.25/0.5/0.75/1; citation snippets with 32/64/128/300-byte limits. Requests are synthetic client-shaped data, not client recordings.

`expected.json` records Node Store results without rounding: ordered decimal-string IDs, lexicalBm25, vectorSquaredL2, fused scores, UTF-8 snippet bytes as base64, UTF-16 highlight ranges, absolute source byte ranges and truncation flags. Absent values are explicit null. IDs/order, BM25, vector squared L2, bytes/ranges/flags are exact; fused scores allow absolute error <= 1e-6. No generations, timings or filesystem identities participate. The baseline is from the current graph-free Store, permitted by the ticket; see evidence for release-package availability and precise source/build provenance.

From the repository root:

```sh
node bindings/node/bench/client-parity.mjs queries
# Explicit baseline update only; ordinary checks never overwrite expected results:
node bindings/node/bench/client-parity.mjs record
node bindings/node/bench/client-parity.mjs check
node bindings/node/bench/client-parity.mjs replay /absolute/path/to/package actual.json
node bindings/node/bench/client-parity.mjs compare tests/fixtures/client-parity-v1/expected.json actual.json
node --test bindings/node/test/client-parity.test.mjs
```

The package must export the existing `openNamespace(root, name, spec, options)` API. `replayQueries(store, queries)` also accepts any already-built Store exposing `query(request)`. The default graph-free runner builds with autoSealRows=30200 (100 meetings per sealed segment), closes and reopens a temporary store, and deletes it after replay. Keep `expected.json` as the ZE-386 baseline; never overwrite it to hide a difference.

ZE-367's unified runner builds the same 151,000 documents, enables graph, and makes 500 relationship-only writes. Note `n` has keyed `PARITY_NOTE` edge `parity-note-n` from document `302*n+301` to `302*n+302`. It creates no helper documents and changes no text, attributes, vectors or revisions. The runner checks every edge, document/relationship counts, and filename/SHA-256 maps of all five `.zseg` files. All 500 queries must agree before attachment, after attachment, and after closing/reopening the same root. The test also compares the preserved ZE-386 baseline.

Released 0.6.0 has no `eligibleIds`. `release-reference` translates only the 50 complete-note eligible sets: it validates all 302 ascending contiguous IDs in the seeded corpus, removes `eligibleIds`, and ANDs the original filter with the note's attribute-1 equality predicate. All other 450 requests stay unchanged. This is an explicit benchmark input translation before search; malformed sets are refused. The translation test checks the entire note-ID set and query equivalence on the graph-free Store.

Capture the published npm reference on the host, from the repository root:

```sh
npm install --prefix /private/tmp/ze367-release --ignore-scripts --no-audit --no-fund @zepdb/zeppelin-embed@0.6.0
node bindings/node/bench/client-parity.mjs release-reference /private/tmp/ze367-release/node_modules/@zepdb/zeppelin-embed tests/fixtures/client-parity-v1/expected-v0.6.0.json
node bindings/node/bench/client-parity.mjs unified ./bindings/node tests/fixtures/client-parity-v1/expected-v0.6.0.json
node --test bindings/node/test/client-parity.test.mjs
```

`expected-v0.6.0.json` is a separate release reference. Its adjacent `expected-v0.6.0.json.provenance.json` records loaded-addon, wrapper, query, generator, translated-query and output SHA-256 hashes plus host settings. The expected source tag is `v0.6.0`, commit `f39087d7b20016138d9a1946055bc229cdffdd23`; this identifies the intended source reference, not a reproducible-build claim about the npm addon. Release addon/output hashes are pending the host capture. The CLI refuses a package whose declarations expose `eligibleIds`, preventing this checkout (also versioned 0.6.0) from becoming the release oracle. No reference is downloaded or silently generated during tests. When the saved reference is absent, only its comparison test skips with the capture command; all three named unified-store/translation tests still run.

Cypher folder eligibility preserves the shipped 65,536-row cap. It queries each distinct requested folder separately with `IN $folders`, `ORDER BY id`, and `maxRows:65536`; each seeded folder contains 15,100 documents. The shipped profile exposes `ze.node_id(d)`, not `id(d)`: its fixed-width 32-digit hexadecimal string preserves all u128 identity bits and sorts in numeric ID order. The collector validates this representation and converts it to `bigint`. It checks each page against the Store's exact folder count and one admitted generation, then merges and sorts IDs, rejecting overlap. Each document has one scalar folder, so these disjoint partitions cover exactly the same set as one large `IN` query; global ascending sort restores that query's order. The test checks each complete folder-ID set against the seeded note/folder assignment, the full 151,000-ID union, multi-folder merging, all 100 fixture folder queries, and intersections with existing eligible sets, including empty, duplicate, unknown and cross-folder IDs. It reuses the verified singleton results for repeated fixture folders. Additional hybrid/snippet requests exercise exact BM25, vector L2, snippet bytes/highlights/ranges/flags and fused tolerance. Row/budget failures propagate; incomplete results never become eligibility filters.

Current query-file SHA-256: `4ae461854f48fb1614050851b15c6cbc4f3213749f68e1255db762d0b7078b5c`.
Current generator SHA-256: `709a671f8da35e244095604a28aa985b54b092bdeddaaaaab8ac8f7ade92f3dc`.
