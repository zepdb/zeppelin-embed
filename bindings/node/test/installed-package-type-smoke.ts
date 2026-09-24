import {
  ABI_VERSION,
  CancellationToken,
  Store,
  idToUuid,
  openNamespace,
  uuidToId,
  type CountGroup,
  type CountResult,
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

const records = openNamespace(
  'typecheck-records',
  'notes',
  { vectorSpace: { dimensions: 2 } },
  { autoSealRows: 2048 },
);
const sealed: bigint = records.seal().generation;
const grouped = records.count({ groupBy: { attributeId: 1, limit: 16 } });
const groups: CountGroup[] = grouped.groups;
const missing: bigint = grouped.missingCount;
const plain: CountResult = records.count({ timestampRange: { start: 0n, end: 1n } });
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
const uuid: string = idToUuid(uuidToId('123e4567-e89b-12d3-a456-426614174000'));

console.log(ABI_VERSION, generation, sealed, groups, missing, plain, hits, mode, lexicalScore, effectiveAlpha, uuid);
token.close();
records.close();
store.close();
