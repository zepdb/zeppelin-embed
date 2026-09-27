import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createRequire } from 'node:module';
import test from 'node:test';
const { declareCascade, deleteCascade, openNamespace, uuidToId } = createRequire(import.meta.url)('..');
const spec = { attributes: [{ id: 1, name: 'parent', type: 'id128', nullable: true }] };
const names = ['notes', 'segments', 'words'];
const participants = names.map(name => ({ name, spec }));
const parent = uuidToId('fedcba98-7654-3210-fedc-ba9876543210');
function seed(root) {
  for (const [index, name] of names.entries()) {
    const store = openNamespace(root, name, spec);
    try {
      store.upsert([1n, 2n].map(n => ({
        id: parent + BigInt(index * 10) + n, text: 'private family text',
        attributes: [{ id: 1, type: 'id128', value: parent + BigInt(Math.max(index - 1, 0) * 10) + n }],
      })));
      store.seal();
    } finally { store.close(); }
  }
}
function declare(root) {
  declareCascade(root, participants, { parentIndex: 0, childIndex: 1, attributeId: 1 });
  declareCascade(root, participants, { parentIndex: 1, childIndex: 2, attributeId: 1 });
}
function state(root) {
  return [...names].reverse().map(name => {
    const offset = BigInt(names.indexOf(name) * 10);
    const store = openNamespace(root, name, spec, { readOnly: true });
    try { return store.get([parent + offset + 1n, parent + offset + 2n]).documents.map(Boolean); }
    finally { store.close(); }
  });
}
const deletes = () => participants.map((p, i) => ({ ...p, deletes: i === 0 ? [parent + 1n] : [] }));

test('cascade declarations survive reopen and delete transitive dependants', () => {
  const root = mkdtempSync(join(tmpdir(), 'ze-cascade-'));
  try {
    seed(root); declare(root);
    // A second child and its descendant must be collected, too; null has no owner.
    for (const [name, id, owner] of [['segments', 99n, parent + 1n], ['words', 100n, 99n]]) {
      const store = openNamespace(root, name, spec);
      try { store.upsert([
        { id, text: 'additional dependant', attributes: [{ id: 1, type: 'id128', value: owner }] },
        { id: 101n, text: 'unowned', attributes: [{ id: 1, type: 'null', value: null }] },
      ]); } finally { store.close(); }
    }
    // Exact redeclaration has no additional effect.
    declare(root);
    assert.equal(deleteCascade(root, deletes()).length, 3);
    for (const [name, id] of [['segments', 99n], ['words', 100n]]) {
      const store = openNamespace(root, name, spec, { readOnly: true });
      try { assert.deepEqual(store.get([id, 101n]).documents.map(Boolean), [false, true]); }
      finally { store.close(); }
    }
    assert.deepEqual(state(root), [[false, true], [false, true], [false, true]]);
    deleteCascade(root, deletes());
    assert.deepEqual(state(root), [[false, true], [false, true], [false, true]]);
  } finally { rmSync(root, { recursive: true, force: true }); }
});

test('cascade rejects cycles, invalid declarations, incomplete participants and open writers without deletion', () => {
  const root = mkdtempSync(join(tmpdir(), 'ze-cascade-invalid-'));
  try {
    seed(root); declare(root);
    assert.throws(() => declareCascade(root, participants, { parentIndex: 2, childIndex: 0, attributeId: 1 }), error => {
      assert.equal(error.code, 'ZE_ERR_CASCADE_CYCLE');
      for (const name of names) assert.match(error.message, new RegExp(name));
      return true;
    });
    assert.throws(() => declareCascade(root, participants, { parentIndex: 0, childIndex: 0, attributeId: 1 }), { code: 'ZE_ERR_CASCADE_CYCLE' });
    assert.throws(() => declareCascade(root, participants, { parentIndex: 0, childIndex: 1, attributeId: 0 }), /id128/);
    assert.throws(() => declareCascade(root, participants, { parentIndex: 9, childIndex: 1, attributeId: 1 }));
    assert.throws(() => declareCascade(root, [{ name: 'missing', spec }, participants[1]], { parentIndex: 0, childIndex: 1, attributeId: 1 }));
    assert.throws(() => deleteCascade(root, deletes().slice(0, 2)), /words/);
    assert.throws(() => deleteCascade(root, [{ ...participants[0], upserts: [{ id: 99n, text: 'forbidden' }] }, ...participants.slice(1)]));
    const writer = openNamespace(root, 'words', spec);
    try { assert.throws(() => deleteCascade(root, deletes()), { code: 'ZE_ERR_STORE_BUSY' }); }
    finally { writer.close(); }
    assert.deepEqual(state(root), [[true, true], [true, true], [true, true]]);
    deleteCascade(root, deletes());
    assert.deepEqual(state(root), [[false, true], [false, true], [false, true]]);
  } finally { rmSync(root, { recursive: true, force: true }); }
});

test('corrupt cascade metadata fails before publication', () => {
  const root = mkdtempSync(join(tmpdir(), 'ze-cascade-corrupt-'));
  try {
    seed(root); declare(root);
    const path = join(root, '.ze-cascades');
    const bytes = readFileSync(path); bytes[bytes.length - 1] ^= 1; writeFileSync(path, bytes);
    assert.throws(() => deleteCascade(root, deletes()), /checksum/);
    assert.deepEqual(state(root), [[true, true], [true, true], [true, true]]);
  } finally { rmSync(root, { recursive: true, force: true }); }
});
