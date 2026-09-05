from pathlib import Path
import json,statistics,math,hashlib,re,sys
r=Path(__file__).parent
f=json.loads(Path('/private/tmp/ze-clean71-4bit-mscs_8rg/faithful/full-fixture.json').read_text())
ids=[q['id'] for q in f['queries']]
source={str(d['id']):d['source_id'] for d in f['documents']}
rels={q['id']:{} for q in f['queries']}
for q in f['qrels']:rels[q['query-id']][q['corpus-id']]=int(q['score'])
def quality(samples):
 ndcg=[];recall=[]
 for s in samples:
  parents=list(dict.fromkeys(source[h['id']] for h in s['hits']))[:10]
  pos={p:g for p,g in rels[s['query']].items() if g>0}
  ideal=sum((2**g-1)/math.log2(i+2) for i,g in enumerate(sorted(pos.values(),reverse=True)[:10]))
  actual=sum((2**pos.get(p,0)-1)/math.log2(i+2) for i,p in enumerate(parents))
  ndcg.append(actual/ideal if ideal else 0);recall.append(len(set(parents)&pos.keys())/len(pos) if pos else 0)
 return {'ndcg_at_10':statistics.mean(ndcg),'recall_at_10':statistics.mean(recall)}
def pct(v,p):return sorted(v)[math.ceil(len(v)*p)-1]
def key(d):return [(s['query'],s['hits']) for s in d['samples']]
def chunks(s):return {(h['id'],h['chunk']) for h in s['hits']}
def comparison(a,b):
 return {'identical_ordered_payload_queries':sum(x['hits']==y['hits'] for x,y in zip(a['samples'],b['samples'])),'chunk_overlap_at_10':statistics.mean(len(chunks(x)&chunks(y))/len(chunks(x)) for x,y in zip(a['samples'],b['samples']))}
complete=json.loads((r/'runs/complete.json').read_text());assert complete=={'complete':True,'processes':24}
receipts=json.loads((r/'runs/receipts.json').read_text());assert len(receipts)==24 and all(x['exit_code']==0 for x in receipts)
data={x['label']:json.loads((r/x['label']/'results.json').read_text()) for x in receipts}
summary={'rows':[],'processes':len(receipts),'timed_api_calls':len(receipts)*len(ids)}
tokens=None;epoch=None
for state in ['all','tomb']:
 for api in ['lexical','hybrid']:
  row={'state':state,'api':api}
  for arm in ['before','after']:
   ds=[data[f'{arm}-{state}-{api}-r{rep}'] for rep in [1,2,3]]
   for d in ds:
    assert [s['query'] for s in d['samples']]==ids and d['warmups']==20
    if tokens is None:tokens=d['token_ids'];epoch=d['epoch']
    assert d['token_ids']==tokens and d['epoch']==epoch
    assert key(d)==key(ds[0]),(arm,state,api,'rep results differ')
    assert all(len(s['hits'])<=10 for s in d['samples'])
   values={}
   for p in [50,95]:
    samples=[pct([s['ms'] for s in d['samples']],p/100) for d in ds]
    values[f'p{p}_ms']=statistics.median(samples);values[f'process_p{p}_ms']=samples
   values['process_max_rss_bytes']=[int(re.search(r'(\d+)\s+maximum resident set size',(r/'runs'/f'{arm}-{state}-{api}-r{rep}.log').read_text())[1]) for rep in [1,2,3]]
   values.update(quality(ds[0]['samples']));row[arm]=values
  row['p95_change_pct']=100*(row['after']['p95_ms']/row['before']['p95_ms']-1)
  row.update(comparison(data[f'before-{state}-{api}-r1'],data[f'after-{state}-{api}-r1']))
  assert row['identical_ordered_payload_queries']==648,(state,api,row)
  summary['rows'].append(row)
  print(state,api,'p95',row['before']['p95_ms'],'->',row['after']['p95_ms'],row['p95_change_pct'])
summary['token_ids_sha256']=hashlib.sha256(json.dumps(tokens,sort_keys=True).encode()).hexdigest();summary['epoch']=epoch
(r/'summary.json').write_text(json.dumps(summary,indent=2))
print('All',summary['timed_api_calls'],'calls preserve full payloads and exact token IDs')
