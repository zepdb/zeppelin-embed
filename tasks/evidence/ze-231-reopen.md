# ZE-231 / ZE-232: reopen and append cost of text-indexed namespaces

Hardware: Apple M3 Max, 128 GB, macOS 27.0, Node 24.21.0, release addon from
`node scripts/build-native.mjs`. The Mac was shared with other builds during
every run (load average 13 to 22 during the later runs), so treat wall-clock
numbers as upper bounds. Rows marked "earlier" ran under lighter load.

Workload: one record-only namespace, one raw-string attribute, documents of
about 60 characters of text (`segment N the quick brown fox jumps over the lazy
dog N%97`), `durability: 'durable'`, `commitTier: 'none'`.

Command, from `bindings/node`:

```bash
node scripts/reopen-bench.mjs --docs <sizes> [--batch 1000] \
  [--auto-seal-rows 2048] [--max-resident-bytes 4294967296] [--package <path>]
```

`reopenMs` is `openNamespace` of the closed store. `append` is mean
milliseconds per document over each tenth of the load.

## Before: published 0.4.2, one upsert per document, nothing sealed

| docs   | reopenMs | append, first to last tenth |
|--------|----------|-----------------------------|
| 3,000  | 1,341.7  | 0.087 to 0.929 ms           |
| 6,000  | 5,404.0  | 0.112 to 1.884 ms           |
| 10,000 | 15,201.3 | 0.176 to 3.200 ms           |

Reopen and append are both quadratic. Profiling (`/usr/bin/sample`) showed:

- WAL replay copied the whole active segment per record
  (`ActiveSegment::insert`/`replace`/`tombstone`), and `replace` retokenized
  every row.
- Every insert ran `ActiveSegment::existing`, a linear scan of the active
  doc ids.
- Every write ran `sealed_document_matches_in`, a scan of every sealed row
  (`SegmentReader::document_version`).

## After, nothing sealed (replay fix only)

One upsert per document:

| docs   | reopenMs |
|--------|----------|
| 3,000  | 24.9     |
| 10,000 | 82.2     |
| 20,000 | 170.8    |

Batches of 1,000 (same WAL records, faster to build):

| docs    | reopenMs | first text query |
|---------|----------|------------------|
| 3,000   | 25.2     | 2.6 ms           |
| 20,000  | 162.4    | 18.1 ms          |
| 100,000 | 827.1    | 93.7 ms          |
| 300,000 | 2,502.7  | 326.6 ms         |

The 300,000 row needs `--max-resident-bytes 4294967296`: the default 512 MiB
budget refuses the copy-on-write active segment near 300k unsealed documents
(`ZE_ERR_BUDGET_EXCEEDED`). What remains is linear tokenization of the
unsealed rows (`stemmer::ends_with`, `analyze_with_policy` on top of the
profile). Sealing removes it.

Appends are still linear in unsealed rows, because live ingest copies the
active segment per batch (copy-on-write for readers): 7.1 ms per document
near 20,000 unsealed single upserts. Sealing bounds this.

## After, `autoSealRows: 2048`, one upsert per document

| docs    | reopenMs | first text query | append across the load | segments |
|---------|----------|------------------|------------------------|----------|
| 3,000   | 12.6     | 2.7 ms           | 0.07 to 0.58 ms        | 2        |
| 20,000  | 26.3     | 16.7 ms          | 0.35 to 0.38 ms        | 10       |
| 100,000 | 63.8     | 80.7 ms          | 0.36 to 0.38 ms        | 49       |
| 300,000 | 135.3    | 243.0 ms         | 0.36 to 0.43 ms        | 147      |

(earlier run; a rerun at load average 22 gave 25 to 224 ms reopen and 0.6 to
1.0 ms appends)

Threshold comparison at 100,000 documents (earlier runs):

| autoSealRows | reopenMs | append            | segments |
|--------------|----------|-------------------|----------|
| 512          | 52.6     | 0.12 to 0.19 ms   | 196      |
| 2048         | 63.8     | 0.36 to 0.38 ms   | 49       |
| 8192         | 54.8     | 1.19 to 1.94 ms   | 13       |

Before the indexed sealed-row lookup, with sealing every 5,000 documents,
appends grew from 0.95 ms to 2.34 ms between 20,000 and 100,000 documents.

## Not fixed here

- `wal.ze` is never truncated after a seal (47 MB at 300,000 documents). Open
  still reads and skips absorbed records, which is linear but cheap.
- Nothing merges sealed segments, so the first text query after open grows
  with segment count (243 ms at 147 segments).

## Gates

- Rust: `ingest::wal_replay_copy_tests::wal_replay_clones_the_active_segment_at_most_once`
  (allocation-audit counter; 44 clones before, at most 1 after),
  `ingest::wal_replay_tests::wal_replay_reopens_the_exact_live_state`,
  `ingest::wal_replay_tests::sealed_revision_lookup_finds_every_sealed_copy_of_an_id`.
- Node (`test/seal.test.mjs`): 3,000 unsealed texted documents reopen under
  300 ms (measured 25 ms, more than 10x headroom), and 10,000 single upserts
  with `autoSealRows: 1000` make exactly 10 segments and reopen under 300 ms.
