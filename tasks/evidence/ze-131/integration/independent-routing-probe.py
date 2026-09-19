from pathlib import Path
import json,os,subprocess,sys,tempfile
root=Path(sys.argv[1]);out=Path(sys.argv[2]);out.mkdir(exist_ok=False)
bin=out/'bin';bin.mkdir()
stubs={
 'uname':'''#!/bin/sh
case "$1" in
 -s) printf '%s\\n' "$ZE_ROUTE_OS" ;;
 -m) printf '%s\\n' "$ZE_ROUTE_ARCH" ;;
 *) exit 91 ;;
esac
''',
 'rustc':'''#!/bin/sh
printf 'rustc 1.93.0\\nhost: %s\\n' "$ZE_ROUTE_RUST_HOST"
''',
 'cargo':'''#!/bin/sh
if [ "$1" = "fmt" ]; then exit 0; fi
printf '%s\\n' "$@" > "$ZE_ROUTE_LOG"
exit 73
''',
 'cargo-llvm-cov':'#!/bin/sh\nexit 92\n',
}
for name,data in stubs.items():p=bin/name;p.write_text(data);p.chmod(0o755)
cases=[
 ('mac-arm','Darwin','arm64','aarch64-apple-darwin',None,True),
 ('mac-intel','Darwin','x86_64','x86_64-apple-darwin',None,False),
 ('linux','Linux','x86_64','x86_64-unknown-linux-gnu',None,False),
 ('arm-target-intel','Darwin','arm64','aarch64-apple-darwin','x86_64-apple-darwin',False),
 ('arm-intel-toolchain','Darwin','arm64','x86_64-apple-darwin',None,False),
]
results=[]
for name,osname,arch,rusthost,target,graph in cases:
 for script in ['ci-gates.sh','coverage.sh','adversarial.sh']:
  tag=name+'-'+script;cmdlog=out/(tag+'.args');env=os.environ.copy();env.update({'PATH':str(bin)+os.pathsep+env['PATH'],'ZE_ROUTE_OS':osname,'ZE_ROUTE_ARCH':arch,'ZE_ROUTE_RUST_HOST':rusthost,'ZE_ROUTE_LOG':str(cmdlog)})
  env.pop('CARGO_BUILD_TARGET',None)
  if target:env['CARGO_BUILD_TARGET']=target
  command=['bash',str(root/'scripts'/script)]
  if script=='adversarial.sh':command+=['smoke','--artifacts',str(out/'artifacts'/name)]
  r=subprocess.run(command,cwd=root,env=env,capture_output=True,text=True)
  (out/(tag+'.log')).write_text(r.stdout+r.stderr)
  args=cmdlog.read_text().splitlines() if cmdlog.exists() else []
  selected=any('graph-result-test-support' in a for a in args)
  result={'case':name,'script':script,'exit':r.returncode,'args':args,'expected_graph':graph,'selected_graph':selected,'pass':r.returncode==73 and selected==graph}
  results.append(result)
(out/'results.json').write_text(json.dumps(results,indent=2)+'\n')
print(json.dumps({'checked':len(results),'passed':sum(r['pass'] for r in results),'mismatches':[{k:r[k] for k in ['case','script','exit','expected_graph','selected_graph']} for r in results if not r['pass']]},indent=2))
