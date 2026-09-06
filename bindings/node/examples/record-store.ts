import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import zeppelinEmbed from '../index.js';
import type {
  Filter,
  NamespaceSpec,
  ScanCursor,
  UpsertDocument,
} from '../index.js';

const { openNamespace } = zeppelinEmbed;

const spec: NamespaceSpec = {
  attributes: [
    { id: 1, name: 'priority', type: 'u64' },
    { id: 2, name: 'reviewed', type: 'bool' },
    { id: 3, name: 'category', type: 'dictionaryString' },
    { id: 4, name: 'project', type: 'rawString', nullable: true },
  ],
  vectorSpace: { dimensions: 2, normalization: 'unitL2' },
};

// Each namespace lives at root/name on disk. The temporary root keeps the
// example self-cleaning; use an application path to reopen the data later.
const root = mkdtempSync(join(tmpdir(), 'zeppelin-node-record-store-'));
const notes = openNamespace(root, 'notes', spec);

try {
  const documents: UpsertDocument[] = [
    {
      id: 101n,
      timestamp: 100n,
      vector: new Float32Array([1, 0]),
      text: 'Plan the product launch',
      attributes: [
        { id: 1, type: 'u64', value: 2n },
        { id: 2, type: 'bool', value: true },
        { id: 3, type: 'string', value: 'work' },
        { id: 4, type: 'string', value: 'zeppelin' },
      ],
    },
    {
      id: 102n,
      timestamp: 300n,
      vector: new Float32Array([0, 1]),
      text: 'Buy oat milk',
      attributes: [
        { id: 1, type: 'u64', value: 1n },
        { id: 2, type: 'bool', value: false },
        { id: 3, type: 'string', value: 'personal' },
        { id: 4, type: 'null', value: null },
      ],
    },
    {
      id: 103n,
      timestamp: 200n,
      vector: new Float32Array([0.8, 0.6]),
      text: 'Review search benchmarks',
      attributes: [
        { id: 1, type: 'u64', value: 3n },
        { id: 2, type: 'bool', value: true },
        { id: 3, type: 'string', value: 'work' },
        { id: 4, type: 'string', value: 'zeppelin' },
      ],
    },
    {
      id: 104n,
      timestamp: 400n,
      vector: new Float32Array([0.6, 0.8]),
      text: 'Book dentist appointment',
      attributes: [
        { id: 1, type: 'u64', value: 2n },
        { id: 2, type: 'bool', value: false },
        { id: 3, type: 'string', value: 'personal' },
        { id: 4, type: 'null', value: null },
      ],
    },
  ];
  const mutation = notes.upsert(documents);
  console.log(`upserted 4 notes at generation ${mutation.generation}`);

  // get preserves caller order and returns null for a missing id.
  const lookup = notes.get([101n, 999n]);
  const [found, missing] = lookup.documents;
  if (found === null || found === undefined) {
    throw new Error('note 101 unexpectedly missing');
  }
  console.log(`get 101: "${found.text}"`);
  if (missing === null) {
    console.log(`get 999: missing (${lookup.missingCount} missing)`);
  }

  // Logical children become one consecutive range in the C ABI's flat node
  // array; callers keep the readable nested form.
  const selected: Filter = {
    op: 'and',
    children: [
      {
        op: 'eq',
        attributeId: 3,
        values: [{ id: 3, type: 'string', value: 'work' }],
      },
      {
        op: 'range',
        attributeId: 1,
        lower: { id: 1, type: 'u64', value: 2n },
        lowerInclusive: true,
        upper: { id: 1, type: 'u64', value: 3n },
        upperInclusive: true,
      },
    ],
  };
  let cursor: ScanCursor | undefined;
  let scanned = 0;
  let pageNumber = 1;
  do {
    const page = notes.scan({
      cursor,
      limit: 1,
      order: 'timestampAscending',
      filter: selected,
    });
    for (const note of page.documents) {
      console.log(
        `scan page ${pageNumber}: note ${note.id} at ${note.timestamp}: "${note.text}"`,
      );
    }
    scanned += page.documents.length;
    cursor = page.cursor ?? undefined;
    pageNumber += 1;
  } while (cursor !== undefined);

  const count = notes.count({ filter: selected });
  console.log(`count: ${count.count} matching notes (scan found ${scanned})`);

  const matches = notes.searchFiltered(
    new Float32Array([1, 0]),
    selected,
    { k: 2, tier: 'exact' },
  );
  for (const [index, hit] of matches.entries()) {
    console.log(`search ${index + 1}: note ${hit.id}, score ${hit.score.toFixed(3)}`);
  }
} finally {
  notes.close();
}

// Omitting a vector space creates a plain record store. Vector search on this
// namespace is rejected with ZE_ERR_NO_VECTOR_SPACE.
const inbox = openNamespace(root, 'inbox', { attributes: [] });
try {
  inbox.upsert([{ id: 201n, timestamp: 500n, text: 'Call Alice' }]);
  const record = inbox.scan({ limit: 10, order: 'timestampAscending' }).documents[0];
  if (record === undefined) {
    throw new Error('record-only scan unexpectedly empty');
  }
  console.log(`record-only scan: note ${record.id}: "${record.text}"`);
} finally {
  inbox.close();
  rmSync(root, { force: true, recursive: true });
}
