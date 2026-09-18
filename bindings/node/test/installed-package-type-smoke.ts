import {
  ABI_VERSION,
  CancellationToken,
  Store,
  openNamespace,
  type Document,
  type QueryResult,
  type SearchHit,
} from '@zepdb/zeppelin-embed';

const documents: readonly Document[] = [
  { id: 1n, revision: 1n, vector: new Float32Array([1, 0]) },
];
const store = new Store('typecheck-index');
const generation: bigint = store.ingest(documents, 2).generation;
const hits: SearchHit[] = store.search(new Float32Array([1, 0]), 1);

const records = openNamespace('typecheck-records', 'notes', {
  vectorSpace: { dimensions: 2 },
});
const token = new CancellationToken();
const result: QueryResult = records.query({
  text: 'harbour',
  vector: new Float32Array([1, 0]),
  k: 5,
  lastAsPrefix: true,
  alpha: 0.5,
  deadlineNs: undefined,
  cancelToken: token,
});
const mode: 'vector' | 'lexical' | 'hybrid' = result.mode;
const lexicalScore: number | undefined = result.hits[0]?.lexicalBm25;
const effectiveAlpha: number | undefined = result.fusion?.effectiveAlpha;

console.log(ABI_VERSION, generation, hits, mode, lexicalScore, effectiveAlpha);
token.close();
records.close();
store.close();
