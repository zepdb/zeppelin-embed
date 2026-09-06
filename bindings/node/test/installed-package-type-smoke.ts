import {
  ABI_VERSION,
  Store,
  type Document,
  type SearchHit,
} from '@zepdb/zeppelin-embed';

const documents: readonly Document[] = [
  { id: 1n, revision: 1n, vector: new Float32Array([1, 0]) },
];
const store = new Store('typecheck-index');
const generation: bigint = store.ingest(documents, 2).generation;
const hits: SearchHit[] = store.search(new Float32Array([1, 0]), 1);

console.log(ABI_VERSION, generation, hits);
store.close();
