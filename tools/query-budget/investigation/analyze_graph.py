from pathlib import Path
import json,math,statistics,random,sys
sys.path.insert(0,'/Users/aghatage/Documents/code/zeppelin-embed/tools/query-budget')
from analyze import quality,distribution
r=Path('/private/tmp/ze-query-investigate-vci05npf')
f=json.loads(Path('/private/tmp/ze-clean71-4bit-mscs_8rg/faithful/full-fixture.json').read_text())
ids=[q['id'] for q in f['queries']]
assert json.loads((r/'graph-build/results.json').read_text())['complete']
assert json.loads((r/'graph-compare-runs/complete.json').read_text())=={'complete':True,'processes':9}
rs=json.loads((r/'graph-compare-runs/receipts.json').read_text());assert len(rs)==9 and all(x['exit_code']==0 for x in rs)
data={x['label']:json.loads((r/x['label']/'results.json').read_text()) for x in rs}
summary={}
for arm in ['scan','graph','exact']:
    ds=[data[f'graph-compare-{arm}-r{x}'] for x in range(1,4)]
    ref=[s['hits'] for s in ds[0]['samples']]
    for d in ds:
        assert [s['query'] for s in d['samples']]==ids
        assert [s['hits'] for s in d['samples']]==ref
        assert all(s['stages_ms'] is None and not s['probes']['pool'] and not s['probes']['native'] for s in d['samples'])
        assert all((s['graph_traversed']>0)==(arm=='graph') for s in d['samples'])
        if arm=='graph': assert all('scan_reason: Some' not in s['plan'] for s in d['samples'])
    row=distribution(ds,lambda s:s['ms']);row.update(quality(ds[0]['samples'],f));summary[arm]=row
scan=data['graph-compare-scan-r1']['samples'];graph=data['graph-compare-graph-r1']['samples'];exact=data['graph-compare-exact-r1']['samples']
assert [s['hits'] for s in scan]==[s['hits'] for s in json.loads((r/'validation-untimed-w12-r1/results.json').read_text())['samples']]
def overlap(a,b):
    k=lambda s:{(h['id'],h['chunk']) for h in s['hits']}
    return statistics.mean(len(k(x)&k(y))/len(k(y)) for x,y in zip(a,b))
summary['graph']['exact_chunk_recall_at_10']=overlap(graph,exact)
summary['scan']['exact_chunk_recall_at_10']=overlap(scan,exact)
summary['graph']['scan_chunk_overlap_at_10']=overlap(graph,scan)
for arm in ['scan','graph']:
    summary[arm]['ndcg_delta_vs_exact']=summary[arm]['ndcg_at_10']-summary['exact']['ndcg_at_10']
summary['graph']['gate_exact_chunk_recall_0_99']=summary['graph']['exact_chunk_recall_at_10']>=.99
summary['graph']['gate_ndcg_loss_at_most_0_005']=summary['graph']['ndcg_delta_vs_exact']>=-.005
for arm in ['graph','exact']:
    summary[arm]['vs_scan']={k:{'absolute_ms':summary[arm][k]-summary['scan'][k],'percent':100*(summary[arm][k]/summary['scan'][k]-1)} for k in ['p50_ms','p95_ms']}
# Paired query bootstrap of graph minus Exact nDCG, not run-to-run quality noise.
deltas=[quality([x],f)['ndcg_at_10']-quality([y],f)['ndcg_at_10'] for x,y in zip(graph,exact)]
rng=random.Random(0x5eed);b=sorted(statistics.mean(rng.choices(deltas,k=len(deltas))) for _ in range(5000))
summary['graph']['ndcg_delta_vs_exact_bootstrap_95']=[b[124],b[4874]]
(r/'graph-summary.json').write_text(json.dumps(summary,indent=2))
print(json.dumps(summary,indent=2))
