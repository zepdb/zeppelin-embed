import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { Store } from '..';

// A store is a persistent directory. This temporary directory keeps the
// example self-cleaning; use an application path to reopen it later.
const directory = mkdtempSync(join(tmpdir(), 'zeppelin-node-ts-'));
const store = new Store(join(directory, 'index'));

try {
  // Applications provide vectors and stable bigint document IDs. The second
  // argument fixes the vector dimension for this ingest batch.
  store.ingest(
    [
      { id: 1n, vector: new Float32Array([0.9, 0.1, 0.05, 0]) },
      { id: 2n, vector: new Float32Array([0.1, 0.9, 0.05, 0]) },
    ],
    4,
  );
  // Query vectors must use the same dimension and embedding space.
  console.log(store.search(new Float32Array([0.88, 0.12, 0.07, 0.02]), 1));
} finally {
  // Close releases the native handle before the example removes its files.
  store.close();
  rmSync(directory, { force: true, recursive: true });
}
