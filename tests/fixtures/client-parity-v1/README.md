# Client parity v1 — ZE-386

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

The package must export the existing `openNamespace(root, name, spec, options)` API. `replayQueries(store, queries)` also accepts any already-built Store exposing `query(request)`; ZE-367 can build the identical corpus, enable graph and write its relationships, then replay without changing expectations. `documentBatches()` and `SPEC` are exported for that setup. The default runner builds with autoSealRows=30200 (100 meetings per sealed segment), closes and reopens a temporary graph-free store; it deletes that store after replay. Commit only generator, query list and expected results, never the generated store. This slice does not enable graph or change legacy namespace readers.
