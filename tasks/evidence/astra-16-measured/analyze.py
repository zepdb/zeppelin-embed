from pathlib import Path
import json,statistics,math
out=Path('tasks/evidence/astra-16-measured')
assert json.loads((out/'complete.json').read_text())=={'complete':True,'processes':6}
data={(arm,rep):json.loads((out/f'{arm}-rep{rep}/results.json').read_text()) for arm in ['before','after'] for rep in [1,2,3]}
def pct(xs,p): return sorted(xs)[math.ceil(len(xs)*p)-1]
table=[];memory=[]
for i in range(3):
 before=data['before',1]['cases'][i]
 for value in data.values():
  case=value['cases'][i]
  for field in ['sealed_rows','sealed_segments','controls']:assert case[field]==before[field],(i,field)
 for metric in ['warm_us','setup_us','present_us']:
  row={'rows':before['sealed_rows'],'segments':before['sealed_segments'],'metric':metric}
  for arm in ['before','after']:
   p50s=[pct(data[arm,r]['cases'][i][metric],.5) for r in [1,2,3]]
   p95s=[pct(data[arm,r]['cases'][i][metric],.95) for r in [1,2,3]]
   row[arm]={'p50_us':statistics.median(p50s),'p95_us':statistics.median(p95s),'process_p50_us':p50s,'process_p95_us':p95s}
  row['p95_delta_us']=row['after']['p95_us']-row['before']['p95_us']
  row['p95_change_percent']=(row['after']['p95_us']/row['before']['p95_us']-1)*100
  table.append(row)
 memory.append({'rows':before['sealed_rows'],'segments':before['sealed_segments'],**{arm:{key:data[arm,1]['cases'][i][key] for key in ['initial_cache_bytes','final_cache_bytes','peak_sampled_cache_bytes']} for arm in ['before','after']}})
receipts=json.loads((out/'run-receipts.json').read_text());assert len(receipts)==6 and all(x['exit_code']==0 for x in receipts)
summary={'table':table,'cache_bytes':memory,'exact_scored_hits_checked':sum(len(h) for d in data.values() for c in d['cases'] for h in c['controls']),'timed_calls':3*6*(128+64*2),'process_memory':{arm:{'rss_process_bytes':[r['maximum_rss_bytes'] for r in receipts if r['arm']==arm],'median_rss_bytes':statistics.median(r['maximum_rss_bytes'] for r in receipts if r['arm']==arm)} for arm in ['before','after']}}
(out/'aggregate.json').write_text(json.dumps(summary,indent=2))
for row in table:print(row['rows'],row['segments'],row['metric'],row['before']['p95_us'],row['after']['p95_us'],round(row['p95_change_percent'],2))
print('memory',memory,summary['process_memory'])
