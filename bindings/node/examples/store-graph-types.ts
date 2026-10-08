import {Store, CancellationToken, GraphPlan} from '..';
const s = new Store('/tmp/graph', {maxResidentBytes: 268435456n});
s.enableGraph();
const items = [{kind: 'node', operation: 'create', namespace: 'docs', key: 'a', revision: 1n, id: 7n, vector: new Float32Array([1, 0]), text: 'amber', timestamp: 1n, attributes: [{id: 1, type: 'i64', value: 4n}], metadata: new Uint8Array([1])}] as const;
s.graphApply(items);
s.graphApplyAsync(items, {signal: new AbortController().signal});
s.cypher('RETURN $xs', {xs: [1n, null, ['a']]});
s.cypherAsync('RETURN 1', {}, {maxRows: 10, signal: new AbortController().signal});
const plan: GraphPlan = {root: 0, operators: [['scanNodes', 0, 'Document']], expressions: [], parameters: [], searches: [], eagerSearches: []};
s.graphQuery(plan);
s.graphQueryAsync(plan, {signal: new AbortController().signal});
s.graphGetNodes([7n], {text: true, vector: true});
s.graphGetNodesAsync([7n], {signal: new AbortController().signal});
s.graphGetRelationships([7n]);
s.graphResources();
const token = new CancellationToken();
s.cypher('RETURN 1', {}, {cancelToken: token});
// @ts-expect-error Plans remain structured data.
s.graphQuery('MATCH (n) RETURN n');
// @ts-expect-error IDs retain all 128 bits.
s.graphGetNodes([7]);
// @ts-expect-error Relationships do not carry document vectors.
s.graphApply([{kind: 'relationship', operation: 'create', namespace: 'edges', key: 'ab', revision: 1n, type: 'LINK', source: 1n, target: 2n, vector: new Float32Array()}]);
// @ts-expect-error Use one interruption source.
s.graphApplyAsync(items, {signal: new AbortController().signal, deadlineNs: 1n});
// @ts-expect-error Caller ID is create-only.
s.graphApply([{...items[0], operation: 'put', expectedId: 7n}]);
token.close(); s.close();
