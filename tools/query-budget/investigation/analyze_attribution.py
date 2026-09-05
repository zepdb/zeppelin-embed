import json,math,statistics
from pathlib import Path
r=Path('/private/tmp/ze-query-investigate-vci05npf')
def pct(xs,q):return sorted(xs)[math.ceil(len(xs)*q)-1]
def dist(ds,f):
 a=[pct([f(s) for s in d['samples']],.5) for d in ds];b=[pct([f(s) for s in d['samples']],.95) for d in ds]
 return {'p50':statistics.median(a),'p95':statistics.median(b),'process_p50':a,'process_p95':b}
if __name__=='__main__':
 assert json.loads((r/'attribution-runs/complete.json').read_text())['complete']
 receipts=json.loads((r/'attribution-runs/receipts.json').read_text());assert len(receipts)==33
 reference=json.loads(Path('/private/tmp/ze-query-budget-hsslyrjg/e3-api-copy-timed-r1/results.json').read_text())['samples']
 ids=[s['query'] for s in reference];keys=lambda ss:[[[h[k] for k in ['id','chunk','revision','score_bits']] for h in s['hits']] for s in ss]
 groups={}
 for rec in receipts:
  assert rec['exit_code']==0;d=json.loads((r/rec['label']/'results.json').read_text());ss=d['samples'];assert [s['query'] for s in ss]==ids
  if rec['arm'].startswith(('core','query')):assert keys(ss)==keys(reference),rec['label']
  for s in ss:
   for p in s['probes']['pool']:
    assert p['total_ns']==sum(p[k] for k in ['setup_ns','dispatch_ns','join_ns','merge_ns'])
    assert all(0<x<y<=p['total_ns'] for x,y in zip(p['worker_start_ns'],p['worker_end_ns']))
   for p in s['probes']['native']:
    assert p['native_inner_ns']==sum(p[k] for k in ['allocation_ns','input_copy_ns','provider_ns','prediction_ns','output_lookup_ns','output_copy_ns'])
    assert p['native_inner_ns']<=p['rust_shim_ns']
  groups.setdefault(rec['arm'],[]).append(d)
 out={}
 for name,ds in groups.items():
  assert len(ds)==3;row=dist(ds,lambda s:s['ms']);p=ds[0]['samples'][0]['probes']
  if p['pool']:
   row['pool_ms']={k:dist(ds,lambda s,k=k:s['probes']['pool'][0][k]/1e6) for k in ['setup_ns','dispatch_ns','join_ns','merge_ns','total_ns']}
   row['slowest_partition_ms']=dist(ds,lambda s:max(y-x for x,y in zip(s['probes']['pool'][0]['worker_start_ns'],s['probes']['pool'][0]['worker_end_ns']))/1e6)
   row['last_worker_start_ms']=dist(ds,lambda s:max(s['probes']['pool'][0]['worker_start_ns'])/1e6)
   row['outside_pool_ms']=dist(ds,lambda s:((s['stages_ms']['retrieval'] if s.get('stages_ms') else s['ms'])-s['probes']['pool'][0]['total_ns']/1e6))
  if p['native']:
   row['native_ms']={k:dist(ds,lambda s,k=k:s['probes']['native'][0][k]/1e6) for k in p['native'][0] if k!='qos'}
   row['native_requested_qos']=sorted({s['probes']['native'][0]['qos'] for d in ds for s in d['samples']})
  if ds[0]['samples'][0].get('stages_ms'):
   row['stages_ms']={k:dist(ds,lambda s,k=k:s['stages_ms'][k]) for k in ds[0]['samples'][0]['stages_ms']}
  out[name]=row;print(name,round(row['p50'],6),round(row['p95'],6),'prediction',row.get('native_ms',{}).get('prediction_ns',{}).get('p50'))
 (r/'attribution-summary.json').write_text(json.dumps(out,indent=2));print('PASS: all repeated hits and probe accounting.')
