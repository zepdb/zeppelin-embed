import {
  ABI_VERSION,
  type GraphMutation,
  type GraphResult,
  type GraphValue,
  CancellationToken,
  Store,
  idToUuid,
  openNamespace,
  uuidToId,
  verify,
  type CountGroup,
  type CountResult,
  type Document,
  type QueryResult,
  type SearchHit,
  type VerifyFindingKind,
  type VerifyReport,
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
  { autoSealRows: 2048, autoMerge: true },
);
const sealed: bigint = records.seal().generation;
const merged: bigint = records.merge().generation;
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

const report: VerifyReport = verify('typecheck-records/notes');
const damage: VerifyFindingKind | undefined = report.findings[0]?.kind;
const offset: bigint | undefined = report.findings[0]?.offset;

console.log(ABI_VERSION, generation, sealed, merged, groups, missing, plain, hits, mode, lexicalScore, effectiveAlpha, uuid);
console.log(report.ok, damage, offset);
token.close();
records.close();
store.close();

const graph = new Store('/unused/typecheck-graph', {maxResidentBytes: 268435456n});
const items: GraphMutation[] = [{ kind: 'node', operation: 'create', namespace: 'notes',
  key: 'first', revision: 1n, labels: ['Note'] }];
const graphReport: GraphResult = graph.graphApply(items);
const graphResult: GraphResult = graph.cypher('MATCH (n) RETURN n');
const values: readonly GraphValue[] = graphResult.rows[0];
void graphReport;
void values;
graph.close();

import { TextStore, TextQueryResult, TextHealthReport, TextIngestReport } from '@zepdb/zeppelin-embed';
async function textExample(bundle: string) {
  const text = await TextStore.openAsync('/tmp/text-store', bundle);
  const ingested: TextIngestReport = await text.ingestAsync([{id: 1n, revision: 1n, text: 'bronze zeppelin'}]);
  const hits: TextQueryResult = await text.queryAsync({text: 'bronze', legs: 'hybrid', tier: 'exact', signal: new AbortController().signal});
  const health: TextHealthReport = text.health();
  text.query({text: 'bronze', legs: 'lexical', deadlineNs: 1000000000n});
  await text.maintainAsync({wallTimeNs: 100000000n, bytes: 67108864n});
  await text.deleteAsync([1n]);
  text.close();
  return {ingested, hits, health};
}
void textExample;
void TextStore.openModel({path: '/tmp/text-model', repo: 'owner/model', revision: '0'.repeat(40),
  manifestSha256: '0'.repeat(64), cacheDir: '/tmp/cache', offline: true,
  onProgress: ({done, total}) => { const progress: bigint = done + total; void progress; }});

void TextStore.openModel({path: '/tmp/default-text-model'});
const defaultModelRepo: string = TextStore.defaultModel.repo;
void defaultModelRepo;
// @ts-expect-error Pins must be supplied together.
void TextStore.openModel({path: '/tmp/partial-text-model', repo: 'owner/model'});
