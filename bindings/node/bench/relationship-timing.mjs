// Run through scripts/cy_time.sh so the addon is rebuilt and identified.
import { createRequire } from 'node:module';
import { SPEC } from './client-parity-generator.mjs';

const require = createRequire(import.meta.url);
const api = require('../index.js');
const store = api.openNamespace(process.argv[2], 'perf', SPEC, { readOnly: true });
const hex = n => BigInt(n).toString(16).padStart(32, '0');
const queries = [
  ['count all nodes', 'MATCH (n) RETURN count(n) AS c', {}],
  ['count Document label', 'MATCH (d:Document) RETURN count(d) AS c', {}],
  ['point lookup by node_id', 'MATCH (d:Document) WHERE ze.node_id(d) = $id RETURN ze.node_id(d) AS id', { id: hex(5000) }],
  ['10 docs, LIMIT 10', 'MATCH (d:Document) RETURN ze.node_id(d) AS id LIMIT 10', {}],
  ['count relationships', 'MATCH ()-[r]->() RETURN count(r) AS c', {}],
  ['count PERF_LINK', 'MATCH ()-[r:PERF_LINK]->() RETURN count(r) AS c', {}],
  ['all 500 rel pairs', 'MATCH (a)-[r:PERF_LINK]->(b) RETURN ze.node_id(a) AS a, ze.node_id(b) AS b', {}],
  ['2-hop count', 'MATCH (a)-[:PERF_LINK]->(b)-[:PERF_LINK]->(c) RETURN count(c) AS c', {}],
  ['incoming count', 'MATCH (a)<-[r:PERF_LINK]-(b) RETURN count(r) AS c', {}],
  ['undirected count', 'MATCH (a)-[r:PERF_LINK]-(b) RETURN count(r) AS c', {}],
  ['labelled start count', 'MATCH (a:Document)-[r:PERF_LINK]->(b) RETURN count(r) AS c', {}],
  ['id-anchored expand', 'MATCH (a)-[r]->(b) WHERE ze.node_id(a) = $id RETURN ze.node_id(b) AS id', { id: hex(1) }],
  ['rel pairs, LIMIT 10', 'MATCH (a)-[r:PERF_LINK]->(b) RETURN ze.node_id(a) AS a, ze.node_id(b) AS b LIMIT 10', {}],
];

try {
  for (const [name, text, params] of queries) {
    const times = [];
    let rows;
    for (let i = 0; i < 3; i++) {
      const start = process.hrtime.bigint();
      const result = store.cypher(text, params, { maxRows: 65536 });
      times.push(Number(process.hrtime.bigint() - start) / 1e6);
      rows = result.rows;
    }
    console.log(JSON.stringify({ name, text, params, milliseconds: times,
      medianMs: [...times].sort((a, b) => a - b)[1], rows: rows.length,
      firstRow: rows[0] }, (_key, value) => typeof value === 'bigint' ? value.toString() : value));
  }
} finally {
  store.close();
}
