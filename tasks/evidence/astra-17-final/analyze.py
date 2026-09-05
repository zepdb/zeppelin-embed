from pathlib import Path
import json, math, statistics as st
root=Path(__file__).parent
runs={a:[json.loads((root/f"{a}-rep{r}/results.json").read_text())["cases"] for r in [1,2,3]] for a in ["before","after"]}
def pct(values,p):return sorted(values)[math.ceil(len(values)*p)-1]
summary=[];queries=0;hits=0
for case in range(6):
 b=runs["before"][0][case]
 key={k:b[k] for k in ["sealed_rows","sealed_segments","live_rows","tombstones"]}
 for arm in runs:
  for run in runs[arm]:
   c=run[case]
   assert all(c[k]==v for k,v in key.items())
   assert c["cold_hits"]==b["cold_hits"]
   for i,w in enumerate(c["workloads"]):
    assert w["controls"]==b["workloads"][i]["controls"],(key,arm,w["name"])
    queries+=len(w["controls"]);hits+=sum(map(len,w["controls"]))
 for wi,w in enumerate(b["workloads"]):
  row={**key,"workload":w["name"]}
  for arm in runs:
   for p in [50,95]:
    values=[pct(run[case]["workloads"][wi]["us"],p/100) for run in runs[arm]]
    row[f"{arm}_p{p}_us"]=st.median(values)
    row[f"{arm}_p{p}_range_us"]=[min(values),max(values)]
  row["p95_change_pct"]=(row["after_p95_us"]/row["before_p95_us"]-1)*100
  summary.append(row)
  print(f'{key["sealed_rows"]:6d}/{key["sealed_segments"]} live={key["live_rows"]:5d} {w["name"]:16s} p95 {row["before_p95_us"]:9.3f} -> {row["after_p95_us"]:9.3f} us {row["p95_change_pct"]:+7.2f}%')
mem=[]
for i in range(6):
 row={k:runs["before"][0][i][k] for k in ["sealed_rows","sealed_segments","tombstones"]}
 for arm in runs:
  row[arm]={k:[run[i][k] for run in runs[arm]] for k in ["cold_us","initial_cache_bytes","final_cache_bytes","peak_sampled_cache_bytes","ingestion_seconds"]}
 mem.append(row)
result={"rows":summary,"memory_and_cold":mem,"exact_controls":True,"queries":queries,"hits":hits}
(root/"summary.json").write_text(json.dumps(result,indent=2))
print("exact controls",queries,"calls",hits,"hits")
