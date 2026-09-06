import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { Store } from '..';

const directory = mkdtempSync(join(tmpdir(), 'zeppelin-node-ts-'));
const store = new Store(join(directory, 'index'));

try {
  store.ingest(
    [
      { id: 1n, vector: new Float32Array([0.9, 0.1, 0.05, 0]) },
      { id: 2n, vector: new Float32Array([0.1, 0.9, 0.05, 0]) },
    ],
    4,
  );
  console.log(store.search(new Float32Array([0.88, 0.12, 0.07, 0.02]), 1));
} finally {
  store.close();
  rmSync(directory, { force: true, recursive: true });
}
