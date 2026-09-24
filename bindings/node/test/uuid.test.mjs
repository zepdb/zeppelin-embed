import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createRequire } from 'node:module';
import test from 'node:test';

const require = createRequire(import.meta.url);
const { idToUuid, openNamespace, uuidToId } = require('..');

const MAX = (1n << 128n) - 1n;
const MASK64 = (1n << 64n) - 1n;

// ZE_TEST_SEED makes a failed run reproducible, as in the Rust suites.
const SEED = BigInt(process.env.ZE_TEST_SEED ?? '0x5eed0230');

/** SplitMix64 over bigint: deterministic, dependency-free. */
function seededRng(seed) {
  let state = seed & MASK64;
  return () => {
    state = (state + 0x9e3779b97f4a7c15n) & MASK64;
    let z = state;
    z = ((z ^ (z >> 30n)) * 0xbf58476d1ce4e5b9n) & MASK64;
    z = ((z ^ (z >> 27n)) * 0x94d049bb133111ebn) & MASK64;
    return z ^ (z >> 31n);
  };
}

function random128(next) {
  return (next() << 64n) | next();
}

/** An independent formatter, so the test does not trust the helper. */
function referenceUuid(id) {
  // Group bit widths of 8-4-4-4-12 hex digits, most significant first.
  const bits = [32n, 16n, 16n, 16n, 48n];
  let shift = 128n;
  return bits
    .map((width) => {
      shift -= width;
      const group = (id >> shift) & ((1n << width) - 1n);
      return group.toString(16).padStart(Number(width / 4n), '0');
    })
    .join('-');
}

const EDGES = [
  0n,
  1n,
  MASK64,
  1n << 64n,
  (1n << 127n) - 1n,
  1n << 127n,
  (1n << 127n) | 1n,
  MAX - 1n,
  MAX,
];

function sampleIds(count) {
  const next = seededRng(SEED);
  const ids = [...EDGES];
  while (ids.length < count) ids.push(random128(next));
  return ids;
}

test('uuid helpers round-trip every 128-bit value exactly', () => {
  for (const id of sampleIds(10_000)) {
    const uuid = idToUuid(id);
    const context = `seed ${SEED} id ${id}`;
    assert.equal(uuid, referenceUuid(id), context);
    assert.equal(uuidToId(uuid), id, context);
    assert.equal(uuidToId(uuid.toUpperCase()), id, context);
  }
  assert.equal(idToUuid(0n), '00000000-0000-0000-0000-000000000000');
  assert.equal(idToUuid(MAX), 'ffffffff-ffff-ffff-ffff-ffffffffffff');
  assert.equal(idToUuid(1n << 127n), '80000000-0000-0000-0000-000000000000');
  assert.equal(
    uuidToId('123E4567-e89b-12D3-a456-426614174000'),
    0x123e4567e89b12d3a456426614174000n,
  );
});

test('uuidToId rejects anything but a canonical 8-4-4-4-12 hex string', () => {
  for (const value of [undefined, null, 1, 1n, {}, ['0'], new String('x')]) {
    assert.throws(
      () => uuidToId(value),
      (error) =>
        error instanceof TypeError && error.code === 'ERR_INVALID_ARG_TYPE',
      String(value),
    );
  }
  const valid = '123e4567-e89b-12d3-a456-426614174000';
  const malformed = [
    '',
    valid.slice(1),
    `${valid}0`,
    valid.replaceAll('-', ''),
    `{${valid}}`,
    `urn:uuid:${valid}`,
    ` ${valid}`,
    `${valid} `,
    `${valid}\n`,
    '123e4567-e89b-12d3-a456-42661417400g',
    '123e4567-e89b-12d3-a456_426614174000',
    '123e456-7e89b-12d3-a456-426614174000',
    '123e4567-e89b12-d3-a456-426614174000',
    '123e4567-e89b-12d3a-456-426614174000',
    '123e4567-e89b-12d3-a4564-26614174000',
    '123e4567+e89b-12d3-a456-426614174000',
    '0x3e4567-e89b-12d3-a456-426614174000',
    '123e4567-e89b-12d3-a456-4266141740٠٠',
  ];
  for (const value of malformed) {
    assert.throws(
      () => uuidToId(value),
      (error) =>
        error instanceof TypeError && error.code === 'ERR_INVALID_ARG_VALUE',
      JSON.stringify(value),
    );
  }
});

test('idToUuid rejects anything but a bigint in 0..2^128-1', () => {
  for (const value of [undefined, null, 0, 1.5, '0', {}]) {
    assert.throws(
      () => idToUuid(value),
      (error) =>
        error instanceof TypeError && error.code === 'ERR_INVALID_ARG_TYPE',
      String(value),
    );
  }
  for (const value of [-1n, MAX + 1n, 1n << 200n, -(1n << 127n)]) {
    assert.throws(
      () => idToUuid(value),
      (error) =>
        error instanceof RangeError && error.code === 'ERR_OUT_OF_RANGE',
      String(value),
    );
  }
});

test('UUID ids round-trip through upsert, get, scan, query and delete', () => {
  const root = mkdtempSync(join(tmpdir(), 'zeppelin-node-uuid-'));
  const spec = {
    attributes: [{ id: 1, name: 'rank', type: 'u64' }],
    vectorSpace: { dimensions: 2, normalization: 'none' },
  };
  const uuids = sampleIds(24).map(idToUuid);
  // Uppercase input is the same id; results always come back lowercase.
  uuids[1] = uuids[1].toUpperCase();
  const canonical = uuids.map((uuid) => uuid.toLowerCase());
  const back = (ids) => ids.map(idToUuid);
  let store;
  try {
    store = openNamespace(root, 'notes', spec);
    store.upsert(
      uuids.map((uuid, index) => ({
        id: uuidToId(uuid),
        vector: new Float32Array([index, 0]),
        text: `harbour lights ${index}`,
        attributes: [{ id: 1, type: 'u64', value: BigInt(index) }],
      })),
    );

    const got = store.get(uuids.map(uuidToId));
    assert.equal(got.missingCount, 0);
    assert.deepEqual(back(got.documents.map((document) => document.id)), canonical);

    const scanned = store.scan({ limit: 1000 });
    assert.equal(scanned.cursor, null);
    assert.deepEqual(
      new Set(back(scanned.documents.map((document) => document.id))),
      new Set(canonical),
    );

    const lexical = store.query({ text: 'harbour', k: 100 });
    assert.deepEqual(
      new Set(back(lexical.hits.map((hit) => hit.id))),
      new Set(canonical),
    );

    // Vector index i sits at distance i from the origin, so rank is order.
    const origin = new Float32Array([0, 0]);
    const vector = store.query({ vector: origin, k: uuids.length, tier: 'exact' });
    assert.deepEqual(back(vector.hits.map((hit) => hit.id)), canonical);
    // search ranks by another metric, so only its id set is compared.
    assert.deepEqual(
      new Set(back(store.search(origin, uuids.length).map((hit) => hit.id))),
      new Set(canonical),
    );
    const filtered = store.searchFiltered(
      origin,
      { op: 'range', attributeId: 1, lower: { id: 1, type: 'u64', value: 20n } },
      { k: uuids.length, tier: 'exact' },
    );
    assert.deepEqual(
      new Set(back(filtered.map((hit) => hit.id))),
      new Set(canonical.slice(20)),
    );

    const removed = [uuids[0], uuids[1], uuids[uuids.length - 1]];
    store.delete(removed.map(uuidToId));
    const afterDelete = store.get(uuids.map(uuidToId));
    assert.equal(afterDelete.missingCount, removed.length);
    assert.deepEqual(
      back(afterDelete.documents.filter(Boolean).map((document) => document.id)),
      canonical.slice(2, -1),
    );
    assert.equal(store.count().count, BigInt(uuids.length - removed.length));
    assert.deepEqual(
      new Set(back(store.scan({ limit: 1000 }).documents.map((document) => document.id))),
      new Set(canonical.slice(2, -1)),
    );
  } finally {
    store?.close();
    rmSync(root, { force: true, recursive: true });
  }
});
