import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { openNamespace, type NamespaceSpec } from '..';

const root = mkdtempSync(join(tmpdir(), 'zeppelin-node-record-store-'));
const spec: NamespaceSpec = {
  attributes: [{ id: 1, name: 'category', type: 'dictionaryString' }],
  vectorSpace: { dimensions: 2 },
};
const store = openNamespace(root, 'documents', spec);

try {
  store.upsert([
    {
      id: 1n,
      vector: new Float32Array([1, 0]),
      text: 'example record',
      attributes: [{ id: 1, type: 'string', value: 'example' }],
    },
  ]);
  console.log(store.get([1n]).documents[0]);
} finally {
  store.close();
  rmSync(root, { force: true, recursive: true });
}
