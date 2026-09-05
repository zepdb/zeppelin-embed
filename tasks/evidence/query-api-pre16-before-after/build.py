from pathlib import Path
import json,subprocess,os,time,hashlib,shutil
r=Path(__file__).parent
receipts=[]
for arm in ['before','recent','after']:
 manifest=r/arm/'tools/matched-api/Cargo.toml';env=os.environ.copy();env['CARGO_TARGET_DIR']=str(r/'target')
 cmd=['cargo','build','--offline','--manifest-path',str(manifest),'--profile','bench','--message-format=json-render-diagnostics']
 print('BUILD',arm,flush=True);start=time.time()
 with (r/(arm+'-build.jsonl')).open('w') as o,(r/(arm+'-build.log')).open('w') as e:
  p=subprocess.run(cmd,env=env,stdout=o,stderr=e)
 row={'arm':arm,'command':cmd,'CARGO_TARGET_DIR':env['CARGO_TARGET_DIR'],'exit_code':p.returncode,'seconds':time.time()-start}
 receipts.append(row);(r/'build-receipts.json').write_text(json.dumps(receipts,indent=2))
 if p.returncode:raise SystemExit(p.returncode)
 binary=r/'target/release/matched-query-api';copy=r/(arm+'-api');shutil.copy2(binary,copy);row['binary_sha256']=hashlib.sha256(copy.read_bytes()).hexdigest()
 feat=subprocess.run(['cargo','tree','--offline','--manifest-path',str(manifest),'-e','features','-i','zeppelin-embed'],env=env,capture_output=True,text=True,check=True)
 (r/(arm+'-features.txt')).write_text(feat.stdout)
 messages=[json.loads(x) for x in (r/(arm+'-build.jsonl')).read_text().splitlines()]
 core=[x for x in messages if x.get('reason')=='compiler-artifact' and x.get('target',{}).get('name')=='zeppelin_embed'];assert len(core)==1 and core[0]['features']==[],core
 row['core_features']=core[0]['features'];row['profile']=core[0]['profile']
 (r/'build-receipts.json').write_text(json.dumps(receipts,indent=2))
 print('DONE',arm,row['seconds'],flush=True)
