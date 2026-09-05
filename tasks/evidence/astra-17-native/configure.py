from pathlib import Path
import json,subprocess,os,hashlib
r=Path(__file__).parent;old=Path("/private/tmp/ze-query-pre16-ielk83g_");faithful=Path("/private/tmp/ze-clean71-4bit-mscs_8rg/faithful")
bundle=faithful/"clean71-fp32-normalizer.zem";fixture=faithful/"full-fixture.json"
env={"ZE_QUERY_COREML":"/private/tmp/ze-clean71-coreml4-_r2_rxbl/clean71-fp16.mlmodelc","ZE_QUERY_COREML_TOKENS":"64"}
for suffix in ["before-all","after-all","before-tomb"]:
 dest=r/("store-"+suffix);assert not dest.exists();subprocess.run(["cp","-cR",str(old/"store-after"),str(dest)],check=True)
command=[str(r/"before-api"),str(bundle),str(fixture),str(r/"prepare-tombstones"),str(r/"store-before-tomb"),"prepare-tombstones"]
with (r/"prepare-tombstones.log").open("x") as log:
 proc=subprocess.run(command,env={**os.environ,**env},stdout=log,stderr=subprocess.STDOUT)
(r/"prepare-receipt.json").write_text(json.dumps({"command":command,"environment":env,"exit_code":proc.returncode},indent=2));assert proc.returncode==0
subprocess.run(["cp","-cR",str(r/"store-before-tomb"),str(r/"store-after-tomb")],check=True)
prepared=json.loads((r/"prepare-tombstones/prepared.json").read_text());data=json.loads(fixture.read_text());data["expected_tombstones"]=prepared["tombstones"]
tombfixture=r/"tombstone-fixture.json";tombfixture.write_text(json.dumps(data,separators=(",",":")))
# Same immutable store bytes enter both arms before the first query process.
stores=[]
for state in ["all","tomb"]:
 def hashes(arm):
  root=r/f"store-{arm}-{state}"
  return {str(p.relative_to(root)):hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted(root.rglob("*")) if p.is_file()}
 before=hashes("before");assert before==hashes("after"),state
 stores.append({"state":state,"files":before})
(r/"store-hashes.json").write_text(json.dumps(stores,indent=2))
cells=[]
for rep,order in enumerate([["before","after"],["after","before"],["before","after"]],1):
 for state in ["all","tomb"]:
  for api in ["lexical","hybrid"]:
   for arm in order:
    label=f"{arm}-{state}-{api}-r{rep}"
    cells.append({"label":label,"arm":arm,"api":api,"state":state,"rep":rep,"command":[str(r/(arm+"-api")),str(bundle),str(fixture if state=="all" else tombfixture),str(r/label),str(r/f"store-{arm}-{state}"),api],"environment":env})
(r/"manifest.json").write_text(json.dumps(cells,indent=2))
print("ready",len(cells),"processes; deleted",len(prepared["deleted_parents"]),"parents,",prepared["tombstones"],"chunks",flush=True)
