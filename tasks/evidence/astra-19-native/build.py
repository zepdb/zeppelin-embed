from pathlib import Path
import hashlib,json,subprocess,os,time,shutil,sys
r=Path(sys.argv[1]);arm=sys.argv[2];tag=sys.argv[3] if len(sys.argv)>3 else arm;manifest=r/arm/'tools/matched-api/Cargo.toml';env=os.environ.copy();env['CARGO_TARGET_DIR']='/private/tmp/ze-astra17-native-mmcjqffv/target'
cmd=['cargo','build','--offline','--manifest-path',str(manifest),'--profile','bench','--message-format=json-render-diagnostics'];t=time.time()
with (r/(tag+'-build.jsonl')).open('x') as out,(r/(tag+'-build.log')).open('x') as err:p=subprocess.run(cmd,env=env,stdout=out,stderr=err)
row={'command':cmd,'CARGO_TARGET_DIR':env['CARGO_TARGET_DIR'],'exit_code':p.returncode,'seconds':time.time()-t};(r/(tag+'-build-receipt.json')).write_text(json.dumps(row,indent=2)+'\n')
if p.returncode:raise SystemExit(p.returncode)
binary=Path(env['CARGO_TARGET_DIR'])/'release/matched-query-api';shutil.copy2(binary,r/(arm+'-api'));row['binary_sha256']=hashlib.sha256((r/(arm+'-api')).read_bytes()).hexdigest()
msgs=[json.loads(x) for x in (r/(tag+'-build.jsonl')).read_text().splitlines()];core=[x for x in msgs if x.get('reason')=='compiler-artifact' and x.get('target',{}).get('name')=='zeppelin_embed'];assert len(core)==1 and core[0]['features']==(['query-timing'] if arm.endswith('-diag') else []),core
row['core_features']=core[0]['features'];row['profile']=core[0]['profile'];(r/(tag+'-build-receipt.json')).write_text(json.dumps(row,indent=2)+'\n');print(json.dumps(row))
