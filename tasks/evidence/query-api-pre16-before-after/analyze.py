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
complete=json.loads((r/'runs/complete.json').read_text());assert complete=={'complete':True,'processes':27}
receipts=json.loads((r/'runs/receipts.json').read_text());assert len(receipts)==27 and all(x['exit_code']==0 for x in receipts)
data={x['label']:json.loads((r/x['label']/'results.json').read_text()) for x in receipts}
summary={'arms':{},'comparisons':{},'processes':len(receipts),'timed_api_calls':len(receipts)*len(ids)}
tokens=None;epoch=None
for arm in ['before','recent','after']:
 summary['arms'][arm]={}
 for api in ['dense','lexical','hybrid']:
  ds=[data[f'{arm}-{api}-r{rep}'] for rep in [1,2,3]]
  for d in ds:
   assert [s['query'] for s in d['samples']]==ids and d['warmups']==20
   if tokens is None:tokens=d['token_ids'];epoch=d['epoch']
   assert d['token_ids']==tokens and d['epoch']==epoch
   assert key(d)==key(ds[0]),(arm,api,'rep results differ')
   assert all(len(s['hits'])<=10 for s in d['samples'])
  p50=[pct([s['ms'] for s in d['samples']],.5) for d in ds];p95=[pct([s['ms'] for s in d['samples']],.95) for d in ds]
  rss=[int(re.search(r'(\d+)\s+maximum resident set size',(r/'runs'/f'{arm}-{api}-r{rep}.log').read_text())[1]) for rep in [1,2,3]]
  row={'p50_ms':statistics.median(p50),'p95_ms':statistics.median(p95),'process_p50_ms':p50,'process_p95_ms':p95,'process_max_rss_bytes':rss,'max_rss_median_bytes':statistics.median(rss),**quality(ds[0]['samples'])}
  summary['arms'][arm][api]=row
for baseline in ['before','recent']:
 summary['comparisons'][baseline+'_to_after']={}
 for api in ['dense','lexical','hybrid']:
  a=summary['arms'][baseline][api];b=summary['arms']['after'][api]
  row={k:{'absolute':b[k]-a[k],'percent':100*(b[k]/a[k]-1)} for k in ['p50_ms','p95_ms','ndcg_at_10','recall_at_10']}
  row.update(comparison(data[f'{baseline}-{api}-r1'],data[f'after-{api}-r1']))
  if baseline=='recent' or api!='hybrid':assert row['identical_ordered_payload_queries']==648,(baseline,api,row)
  summary['comparisons'][baseline+'_to_after'][api]=row
summary['token_ids_sha256']=hashlib.sha256(json.dumps(tokens,sort_keys=True).encode()).hexdigest();summary['epoch']=epoch
(r/'summary.json').write_text(json.dumps(summary,indent=2));print(json.dumps(summary,indent=2))
