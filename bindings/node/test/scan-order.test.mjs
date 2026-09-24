/**
 * `Store.scan` ordered by a declared numeric attribute (ZE-219).
 */
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createRequire } from 'node:module';
import test from 'node:test';

const require = createRequire(import.meta.url);
const { ZeppelinError, openNamespace } = require('..');

const spec = {
  attributes: [
    { id: 1, name: 'startMs', type: 'u64', nullable: true },
    { id: 2, name: 'position', type: 'i64', nullable: true },
    { id: 3, name: 'score', type: 'f64', nullable: true },
    { id: 4, name: 'pinned', type: 'bool', nullable: true },
  ],
};

function withStore(prefix, body) {
  const root = mkdtempSync(join(tmpdir(), prefix));
  const store = openNamespace(root, 'ordered', spec);
  try {
    body(store);
  } finally {
    store.close();
    rmSync(root, { force: true, recursive: true });
  }
}

function numbers(id, startMs, position, score) {
  return {
    id,
    attributes: [
      { id: 1, type: 'u64', value: startMs },
      { id: 2, type: 'i64', value: position },
      { id: 3, type: 'f64', value: score },
    ],
  };
}

function scanAll(store, order, limit) {
  const ids = [];
  let cursor;
  do {
    const page = store.scan({ limit, order, cursor, fields: {} });
    assert.ok(page.documents.length <= limit);
    ids.push(...page.documents.map((document) => document.id));
    cursor = page.cursor ?? undefined;
  } while (cursor !== undefined);
  return ids;
}

function invalid(message) {
  return (error) =>
    error instanceof ZeppelinError &&
    error.code === 'ZE_ERR_INVALID_ARGUMENT' &&
    error.message.includes(message);
}

test('scan orders by u64, i64 and f64 attributes with stable pagination', () => {
  withStore('zeppelin-node-scan-order-', (store) => {
    store.upsert([
      numbers(5n, 18446744073709551615n, -7n, -0),
      numbers(3n, 2n, -9223372036854775808n, Number.NaN),
    ]);
    store.seal();
    store.upsert([
      numbers(4n, 2n, 9n, 0),
      { id: 1n },
      numbers(2n, 0n, -7n, Number.NEGATIVE_INFINITY),
    ]);

    const cases = [
      [1, 'ascending', [2n, 3n, 4n, 5n, 1n]],
      [1, 'descending', [5n, 3n, 4n, 2n, 1n]],
      [2, 'ascending', [3n, 2n, 5n, 4n, 1n]],
      [2, 'descending', [4n, 2n, 5n, 3n, 1n]],
      // -0 equals +0 (doc id breaks the tie); NaN and missing sort last.
      [3, 'ascending', [2n, 4n, 5n, 1n, 3n]],
      [3, 'descending', [4n, 5n, 2n, 1n, 3n]],
    ];
    for (const [attributeId, direction, expected] of cases) {
      for (const limit of [1, 2, 10]) {
        assert.deepEqual(
          scanAll(store, { attributeId, direction }, limit),
          expected,
          `attribute ${attributeId} ${direction} limit ${limit}`,
        );
      }
    }
    assert.deepEqual(scanAll(store, 'storage', 2), [5n, 3n, 4n, 1n, 2n]);
  });
});

test('scan rejects invalid attribute orders with precise errors', () => {
  withStore('zeppelin-node-scan-order-invalid-', (store) => {
    store.upsert([numbers(1n, 1n, 1n, 1)]);
    assert.throws(
      () => store.scan({ order: { attributeId: 9, direction: 'ascending' } }),
      invalid('scan order attribute 9 is not a declared attribute'),
    );
    assert.throws(
      () => store.scan({ order: { attributeId: 4, direction: 'descending' } }),
      invalid(
        'scan order attribute 4 has type Bool; only u64, i64 and f64 attributes are orderable',
      ),
    );
    assert.throws(
      () => store.scan({ order: { attributeId: 0, direction: 'ascending' } }),
      invalid('scan order attribute 0 is the document timestamp; use a timestamp order'),
    );
    assert.throws(
      () => store.scan({ order: { attributeId: 1, direction: 'up' } }),
      (error) =>
        error instanceof RangeError &&
        error.message === "scan order direction must be 'ascending' or 'descending'",
    );
    for (const attributeId of [-1, 1.5, 2 ** 32, '1']) {
      assert.throws(
        () => store.scan({ order: { attributeId, direction: 'ascending' } }),
        (error) =>
          (error instanceof RangeError || error instanceof TypeError) &&
          error.message.startsWith('scan order attributeId must be'),
        `attributeId ${String(attributeId)}`,
      );
    }
    assert.throws(
      () => store.scan({ order: { direction: 'ascending' } }),
      (error) => error instanceof TypeError && error.message === 'attributeId is required',
    );
    assert.throws(
      () => store.scan({ order: 7 }),
      (error) =>
        error instanceof TypeError &&
        error.message === 'scan order must be a string or an attribute order object',
    );
  });
});

test('a cursor from a different order is rejected, and writes make it stale', () => {
  withStore('zeppelin-node-scan-order-cursor-', (store) => {
    store.upsert([numbers(1n, 1n, 1n, 1), numbers(2n, 2n, 2n, 2)]);
    const byStart = { attributeId: 1, direction: 'ascending' };
    const first = store.scan({ limit: 1, order: byStart });
    assert.notEqual(first.cursor, null);
    const others = [
      { attributeId: 1, direction: 'descending' },
      { attributeId: 2, direction: 'ascending' },
      'storage',
      'timestampDescending',
    ];
    for (const order of others) {
      assert.throws(
        () => store.scan({ limit: 1, order, cursor: first.cursor }),
        invalid(
          'scan cursor was issued for order Attribute { column: ColumnId(1), ' +
            'direction: Ascending }, but the request orders by',
        ),
        JSON.stringify(order),
      );
    }
    const storage = store.scan({ limit: 1 });
    assert.throws(
      () => store.scan({ limit: 1, order: byStart, cursor: storage.cursor }),
      invalid('scan cursor was issued for order Storage, but the request orders by'),
    );

    store.upsert([numbers(3n, 3n, 3n, 3)]);
    assert.throws(
      () => store.scan({ limit: 1, order: byStart, cursor: first.cursor }),
      (error) => error instanceof ZeppelinError && error.code === 'ZE_ERR_SCAN_STALE',
    );
  });
});
